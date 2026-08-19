//! Command-line surface for tub.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Interactive Rust terminal client for DeepSeek Harness.
///
/// tub spawns a DeepSeek Harness SDK runtime as a subprocess, drives it over
/// the JSON-RPC stdio protocol, and owns the runtime lifecycle: spawn ->
/// initialize -> prompt -> collect-to-idle -> shutdown -> reap.
#[derive(Debug, Parser)]
#[command(name = "tub", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a single prompt headlessly (no TUI) and print the turn.
    Run(RunArgs),
    /// Start the interactive terminal UI (the default command).
    Tui(TuiArgs),
}

/// Options shared by the headless runner and the TUI.
#[derive(Debug, Clone, Args)]
pub struct SharedArgs {
    /// Path to a DeepSeek Harness checkout (required; src mode needs its
    /// node_modules, lib mode its built lib). Env: TUB_CHECKOUT, DSH_CHECKOUT.
    #[arg(long, env = "TUB_CHECKOUT")]
    pub checkout: Option<PathBuf>,

    /// The runtime cordis.yml path (positional argv[2] for the runtime; its
    /// own DSH_CORDIS_CONFIG env wins when set). Default: TUB_CORDIS_CONFIG,
    /// then ./cordis.yml, then the cordis.yml shipped beside the binary.
    #[arg(long, env = "TUB_CORDIS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Runtime launch mode: 'src' boots the jsonrpc-demo bin through tsx,
    /// 'lib' boots its built lib under plain Node. Env: DSH_EXAMPLE_MODE.
    #[arg(long, value_enum)]
    pub runtime_mode: Option<String>,

    /// Provider route for SDK-created agents.
    #[arg(long, env = "TUB_PROVIDER", default_value = "deepseek-official")]
    pub provider: String,

    /// Model for SDK-created agents.
    #[arg(long, env = "TUB_MODEL", default_value = "deepseek-v4-flash")]
    pub model: String,

    /// Maximum output tokens for each conversation-model request.
    #[arg(long, env = "TUB_MAX_TOKENS")]
    pub max_tokens: Option<u64>,

    /// Workspace directory recorded on every SDK-created session (default:
    /// the current directory).
    #[arg(long, env = "TUB_CWD")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub shared: SharedArgs,

    /// The prompt text (one text content block).
    #[arg(long, short = 'p', required_unless_present = "file")]
    pub prompt: Option<String>,

    /// Read the prompt from this file ('-' for stdin).
    #[arg(long, short = 'f')]
    pub file: Option<PathBuf>,

    /// Session id to run on; omitted mints a fresh session per call.
    #[arg(long)]
    pub session: Option<String>,

    /// Print each wire event as a JSON line as it arrives.
    #[arg(long)]
    pub events: bool,

    /// Print the complete RunResult as JSON before exiting.
    #[arg(long)]
    pub json: bool,

    /// Bound (ms) on the protocol shutdown exchange (default 1000).
    #[arg(long, default_value_t = 1_000)]
    pub shutdown_timeout_ms: u64,
}

impl SharedArgs {
    /// Build from environment variables alone (used when tub runs with no
    /// subcommand, where clap never sees the flattened args).
    pub fn from_env() -> Self {
        let env_opt = |key: &str| std::env::var_os(key).map(|v| PathBuf::from(v));
        let env_u64 = |key: &str| std::env::var(key).ok().and_then(|v| v.parse().ok());
        Self {
            checkout: env_opt("TUB_CHECKOUT"),
            config: env_opt("TUB_CORDIS_CONFIG"),
            runtime_mode: std::env::var("DSH_EXAMPLE_MODE").ok(),
            provider: std::env::var("TUB_PROVIDER")
                .unwrap_or_else(|_| "deepseek-official".to_string()),
            model: std::env::var("TUB_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string()),
            max_tokens: env_u64("TUB_MAX_TOKENS"),
            cwd: env_opt("TUB_CWD"),
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct TuiArgs {
    #[command(flatten)]
    pub shared: SharedArgs,

    /// Session id to open the TUI on; omitted mints a fresh session.
    #[arg(long)]
    pub session: Option<String>,
}
