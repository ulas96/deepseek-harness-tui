//! Resolution of tub's runtime configuration: checkout path, cordis.yml
//! path, launch mode, route, and workspace — from CLI flags, environment
//! variables, and checked-in defaults. All failures are loud: a typo'd
//! checkout must not silently produce a broken spawn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use dsh_harness_client::launch::{require_absolute, resolve_launch, RuntimeMode};

use crate::cli::SharedArgs;

/// tub's resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Absolute path to the DeepSeek Harness checkout.
    pub checkout: PathBuf,
    /// Absolute path to the runtime cordis.yml.
    pub config: PathBuf,
    /// Which artifact the runtime bin is booted from.
    pub mode: RuntimeMode,
    /// Provider route for SDK-created agents.
    pub provider: String,
    /// Model for SDK-created agents.
    pub model: String,
    /// Maximum output tokens for each conversation-model request.
    pub max_tokens: Option<u64>,
    /// Absolute workspace directory recorded on every SDK-created session.
    pub cwd: PathBuf,
}

impl RuntimeConfig {
    /// Resolve from CLI args plus environment. Returns a descriptive error
    /// for the first missing or invalid piece.
    pub fn resolve(shared: &SharedArgs) -> Result<Self, String> {
        let mode = match &shared.runtime_mode {
            Some(raw) => RuntimeMode::parse(raw)?,
            None => match std::env::var("DSH_EXAMPLE_MODE") {
                Ok(raw) => RuntimeMode::parse(&raw)?,
                Err(_) => RuntimeMode::default(),
            },
        };
        let checkout = resolve_checkout(shared, mode)?;
        // Validate the launch artifacts exist before we commit to the config
        // (fail loudly instead of spawning a broken runtime).
        resolve_launch(&checkout, mode)?;
        let config = resolve_config(shared)?;
        let cwd = match &shared.cwd {
            Some(path) => require_absolute(path)?,
            None => std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?,
        };
        Ok(Self {
            checkout,
            config,
            mode,
            provider: shared.provider.clone(),
            model: shared.model.clone(),
            max_tokens: shared.max_tokens,
            cwd,
        })
    }
}

fn resolve_checkout(shared: &SharedArgs, mode: RuntimeMode) -> Result<PathBuf, String> {
    if let Some(path) = &shared.checkout {
        return require_absolute(path);
    }
    if let Some(raw) = std::env::var_os("DSH_CHECKOUT") {
        return require_absolute(PathBuf::from(raw).as_path());
    }

    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    discover_checkout(&cwd, home.as_deref(), mode).ok_or_else(|| {
        "no DeepSeek Harness checkout found; pass --checkout or set TUB_CHECKOUT/DSH_CHECKOUT"
            .to_string()
    })
}

fn discover_checkout(cwd: &Path, home: Option<&Path>, mode: RuntimeMode) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for ancestor in cwd.ancestors() {
        candidates.push(ancestor.to_path_buf());
        candidates.push(ancestor.join("deepseek-harness"));
    }
    if let Some(home) = home {
        for relative in [
            "deepseek-harness",
            "Documents/Github/deepseek-harness",
            "Documents/GitHub/deepseek-harness",
            "Developer/deepseek-harness",
            "dev/deepseek-harness",
            "src/deepseek-harness",
            "Code/deepseek-harness",
            "code/deepseek-harness",
        ] {
            candidates.push(home.join(relative));
        }
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.clone()))
        .find(|candidate| checkout_supports_mode(candidate, mode))
        .and_then(|candidate| std::path::absolute(candidate).ok())
}

fn checkout_supports_mode(checkout: &Path, mode: RuntimeMode) -> bool {
    match mode {
        RuntimeMode::Src => {
            checkout
                .join("packages/examples/jsonrpc-demo/src/bin.ts")
                .is_file()
                && checkout.join("node_modules/tsx/dist/loader.mjs").is_file()
        }
        RuntimeMode::Lib => checkout
            .join("packages/examples/jsonrpc-demo/lib/bin.js")
            .is_file(),
    }
}

/// The runtime cordis.yml: --config, TUB_CORDIS_CONFIG,
/// DSH_CORDIS_CONFIG, ./cordis.yml, the file shipped beside the binary, then
/// tub's embedded default.
fn resolve_config(shared: &SharedArgs) -> Result<PathBuf, String> {
    if let Some(path) = &shared.config {
        return require_absolute(path);
    }
    if let Some(raw) = std::env::var_os("TUB_CORDIS_CONFIG") {
        return require_absolute(PathBuf::from(raw).as_path());
    }
    if let Some(raw) = std::env::var_os("DSH_CORDIS_CONFIG") {
        return require_absolute(PathBuf::from(raw).as_path());
    }
    let cwd_config = std::env::current_dir()
        .map(|dir| dir.join("cordis.yml"))
        .map_err(|e| format!("no working directory: {e}"))?;
    if cwd_config.is_file() {
        return std::path::absolute(&cwd_config).map_err(|e| e.to_string());
    }
    let exe = std::env::current_exe().map_err(|e| format!("no executable path: {e}"))?;
    let shipped = exe
        .parent()
        .map(|dir| dir.join("cordis.yml"))
        .unwrap_or_else(|| PathBuf::from("cordis.yml"));
    if shipped.is_file() {
        return std::path::absolute(&shipped).map_err(|e| e.to_string());
    }
    materialize_bundled_config(&cache_root())
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("tub")
}

fn materialize_bundled_config(cache: &Path) -> Result<PathBuf, String> {
    const BUNDLED: &str = include_str!("../../../cordis.yml");
    std::fs::create_dir_all(cache)
        .map_err(|error| format!("cannot create config cache {}: {error}", cache.display()))?;
    let path = cache.join(format!("cordis-{}.yml", env!("CARGO_PKG_VERSION")));
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(BUNDLED) {
        std::fs::write(&path, BUNDLED)
            .map_err(|error| format!("cannot write bundled config {}: {error}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_checkout(root: &Path, mode: RuntimeMode) {
        let relative = match mode {
            RuntimeMode::Src => "packages/examples/jsonrpc-demo/src/bin.ts",
            RuntimeMode::Lib => "packages/examples/jsonrpc-demo/lib/bin.js",
        };
        let bin = root.join(relative);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(bin, "").unwrap();
        if mode == RuntimeMode::Src {
            let loader = root.join("node_modules/tsx/dist/loader.mjs");
            std::fs::create_dir_all(loader.parent().unwrap()).unwrap();
            std::fs::write(loader, "").unwrap();
        }
    }

    #[test]
    fn discovers_checkout_next_to_current_project() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("some-project");
        let checkout = temp.path().join("deepseek-harness");
        std::fs::create_dir_all(&project).unwrap();
        fake_checkout(&checkout, RuntimeMode::Src);

        assert_eq!(
            discover_checkout(&project, None, RuntimeMode::Src),
            Some(checkout)
        );
    }

    #[test]
    fn discovers_checkout_in_common_home_location() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("unrelated/project");
        let checkout = temp.path().join("Documents/Github/deepseek-harness");
        std::fs::create_dir_all(&project).unwrap();
        fake_checkout(&checkout, RuntimeMode::Lib);

        assert_eq!(
            discover_checkout(&project, Some(temp.path()), RuntimeMode::Lib),
            Some(checkout)
        );
    }

    #[test]
    fn bundled_config_is_materialized_once_and_refreshed() {
        let temp = tempfile::tempdir().unwrap();
        let path = materialize_bundled_config(temp.path()).unwrap();
        let bundled = include_str!("../../../cordis.yml");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bundled);

        std::fs::write(&path, "stale").unwrap();
        assert_eq!(materialize_bundled_config(temp.path()).unwrap(), path);
        assert_eq!(std::fs::read_to_string(path).unwrap(), bundled);
    }
}
