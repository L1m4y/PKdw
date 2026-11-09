use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "pkdw",
    version,
    about = "A self-contained PikPak downloader for the terminal",
    long_about = "PKdw downloads PikPak share links and your own drive files without a browser.\n\
                  It is a single Rust binary: no fish, no Python, no curl, no tqdm.",
    propagate_version = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub output: OutputArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args, Clone)]
pub struct OutputArgs {
    /// Suppress progress bars and non-error messages.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Print machine-readable JSON where supported.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download a share link or a file/folder from your drive.
    #[command(visible_alias = "download")]
    Get(GetArgs),

    /// Inspect a share link or a drive path without downloading.
    Info(InfoArgs),

    /// List a folder in your PikPak drive.
    Ls(LsArgs),

    /// Save or rotate the PikPak refresh token.
    Auth(AuthArgs),

    /// Show the authenticated account and its quota.
    Whoami,

    /// Run environment and configuration checks.
    Doctor,

    /// Install a `pkdw` symlink into PATH.
    Install(InstallArgs),

    /// Show paths, device identity, and configuration status.
    Config,

    /// Generate shell completions.
    Completions(CompletionsArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Quality {
    /// Original file as uploaded (recommended when available).
    #[default]
    Original,
    /// 1080p transcode.
    #[value(name = "1080p")]
    P1080,
    /// 720p transcode.
    #[value(name = "720p")]
    P720,
    /// 480p transcode.
    #[value(name = "480p")]
    P480,
}

impl Quality {
    pub fn key(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// Share URL, share id, or drive path (for example /Movies/file.mkv).
    pub target: String,

    /// Directory for downloaded files (default: <program dir>/downloads).
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Preferred stream quality.
    #[arg(short, long, value_enum, default_value_t = Quality::Original)]
    pub quality: Quality,

    /// Download only the largest file with a known size in a share.
    /// Fails if every candidate has an unknown size.
    #[arg(long)]
    pub largest: bool,

    /// Skip files smaller than this many MiB. Files with an unknown size are skipped.
    #[arg(long, value_name = "MIB", default_value_t = 0)]
    pub min_size: u64,

    /// Only download files whose name contains this text (case-insensitive).
    #[arg(long, value_name = "TEXT")]
    pub name: Option<String>,

    /// Pass code for a protected share.
    #[arg(long, value_name = "CODE")]
    pub pass_code: Option<String>,

    /// Copy each file to your drive first, then download the full original.
    /// Requires authentication and enough drive quota.
    #[arg(long)]
    pub restore: bool,

    /// After a successful --restore download, delete the copy from your drive.
    #[arg(long, requires = "restore")]
    pub delete_after: bool,

    /// Parallel range requests per file.
    #[arg(short = 'j', long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(1..=128))]
    pub connections: u16,

    /// Size of each range request in MiB.
    #[arg(long, value_name = "MIB", default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub chunk_size: u64,

    /// Extra retry rounds after the per-segment retries are exhausted.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=20))]
    pub rounds: u16,

    /// Re-download from scratch, discarding resume state.
    #[arg(long)]
    pub overwrite: bool,

    /// Resolve and list what would be downloaded, but do not transfer data.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Share URL, share id, or drive path.
    pub target: String,

    /// Pass code for a protected share.
    #[arg(long, value_name = "CODE")]
    pub pass_code: Option<String>,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Drive path (default: /).
    #[arg(default_value = "/")]
    pub path: String,

    /// Maximum number of entries per folder page.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
}

#[derive(Debug, Args)]
pub struct AuthArgs {
    /// Refresh token supplied on the command line (visible in shell history).
    #[arg(long, value_name = "TOKEN", conflicts_with_all = ["from_clipboard", "stdin", "logout"])]
    pub token: Option<String>,

    /// Read the refresh token from the system clipboard.
    #[arg(long, conflicts_with_all = ["token", "stdin", "logout"])]
    pub from_clipboard: bool,

    /// Read the refresh token from standard input.
    #[arg(long, conflicts_with_all = ["token", "from_clipboard", "logout"])]
    pub stdin: bool,

    /// Remove the stored refresh token.
    #[arg(long, conflicts_with_all = ["token", "from_clipboard", "stdin"])]
    pub logout: bool,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Destination directory. Default: ~/.local/bin (or PKDW_BIN_DIR).
    #[arg(long, value_name = "DIR")]
    pub to: Option<std::path::PathBuf>,

    /// Replace an existing pkdw entry.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}
