//! Resolution of tub's runtime configuration: checkout path, cordis.yml
//! path, launch mode, route, and workspace — from CLI flags, environment
//! variables, and checked-in defaults. All failures are loud: a typo'd
//! checkout must not silently produce a broken spawn.

use std::path::PathBuf;

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
        let checkout = match &shared.checkout {
            Some(path) => require_absolute(path)?,
            None => match std::env::var_os("DSH_CHECKOUT") {
                Some(raw) => require_absolute(PathBuf::from(raw).as_path())?,
                None => {
                    return Err(
                        "no DeepSeek Harness checkout: pass --checkout, set TUB_CHECKOUT, or set DSH_CHECKOUT"
                            .to_string(),
                    )
                }
            },
        };
        let mode = match &shared.runtime_mode {
            Some(raw) => RuntimeMode::parse(raw)?,
            None => match std::env::var("DSH_EXAMPLE_MODE") {
                Ok(raw) => RuntimeMode::parse(&raw)?,
                Err(_) => RuntimeMode::default(),
            },
        };
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

/// The runtime cordis.yml: --config, TUB_CORDIS_CONFIG, ./cordis.yml, then
/// the cordis.yml shipped beside the binary. The runtime's own discovery
/// still applies inside the child (DSH_CORDIS_CONFIG env wins over the
/// positional argv tub passes).
fn resolve_config(shared: &SharedArgs) -> Result<PathBuf, String> {
    if let Some(path) = &shared.config {
        return require_absolute(path);
    }
    if let Some(raw) = std::env::var_os("TUB_CORDIS_CONFIG") {
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
    Err(format!(
        "no cordis.yml found: pass --config, set TUB_CORDIS_CONFIG, or place one at {} (tub ships one in its repository root)",
        cwd_config.display()
    ))
}
