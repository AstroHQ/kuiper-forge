//! `kuiper-forge setup`: takes a coordinator from nothing to ready to serve.
//!
//! Anything not given as a flag is asked for. With `--yes`, or without a terminal to ask on, what's missing gets its
//! default, or an error naming the flag if it has none.
//!
//! It's safe to run again: an existing CA is never replaced, since that would cut off every registered agent. Without
//! `--force` an existing config is only replaced if the operator says so, and then it's kept as a backup. Certificates
//! and the admin user are filled in either way.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
#[cfg(not(unix))]
use dialoguer::Password;
use dialoguer::theme::{ColorfulTheme, Theme};
use dialoguer::{Confirm, Input, Select};
use rand::RngCore;

use crate::admin::AdminAuthStore;
use crate::auth::{generate_server_cert, init_ca, write_private_key};
use crate::config::{
    AdminConfig, Config, DatabaseConfig, ForgeConfig, ForgeKindConfig, ForgejoConfig, GitHubConfig,
    GrpcConfig, MetricsConfig, PollingConfig, ProvisioningConfig, ProvisioningMode, RunnerScope,
    ServerTrustMode, TlsConfig, WebhookConfig,
};
use crate::db::Database;
use crate::tls::{server_cert_name, server_identity_problem};

const DEFAULT_LISTEN: &str = "0.0.0.0:9443";
const DEFAULT_ORG: &str = "CI Runner";
const DEFAULT_ADMIN_USER: &str = "admin";

/// Where the admin password comes from when there's nobody to ask.
const ADMIN_PASSWORD_ENV: &str = "KUIPER_ADMIN_PASSWORD";

/// Which forge to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ForgeChoice {
    Github,
    Forgejo,
    /// None yet: `serve --dry-run` works, and a forge can be added to the config later
    None,
}

/// How runners get created, see [`ProvisioningMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeChoice {
    Polling,
    Webhook,
    FixedCapacity,
}

#[derive(Debug, Default, Clone, clap::Args)]
pub struct SetupArgs {
    /// Hostname or IP agents will connect to, for the server certificate
    #[arg(long)]
    pub hostname: Option<String>,

    /// Organization name for a new CA certificate
    #[arg(long)]
    pub org: Option<String>,

    /// Address to listen on
    #[arg(long)]
    pub listen: Option<std::net::SocketAddr>,

    /// Forge to serve. Implied by the flags of one
    #[arg(long, value_enum)]
    pub forge: Option<ForgeChoice>,

    /// GitHub App ID
    #[arg(long)]
    pub github_app_id: Option<u64>,

    /// Path to the GitHub App's private key (PEM)
    #[arg(long)]
    pub github_key: Option<PathBuf>,

    /// Forgejo base URL, like https://git.example.com
    #[arg(long)]
    pub forgejo_url: Option<String>,

    /// Environment variable holding the Forgejo access token
    #[arg(long)]
    pub forgejo_token_env: Option<String>,

    /// File holding the Forgejo access token
    #[arg(long)]
    pub forgejo_token_path: Option<PathBuf>,

    /// Forgejo scope to poll: org, owner/repo, @user or *. Repeat for several
    #[arg(long = "forgejo-scope")]
    pub forgejo_scopes: Vec<String>,

    /// How runners get created. Forgejo only supports polling
    #[arg(long, value_enum)]
    pub mode: Option<ModeChoice>,

    /// Where fixed-capacity runners register: an organization or owner/repo. Implies --mode fixed-capacity
    #[arg(long)]
    pub runner_scope: Option<String>,

    /// Enable the admin web UI with this user. The password comes from KUIPER_ADMIN_PASSWORD, or is asked for
    #[arg(long, conflicts_with = "no_admin")]
    pub admin_user: Option<String>,

    /// Leave the admin web UI off
    #[arg(long)]
    pub no_admin: bool,

    /// PostgreSQL host
    #[cfg(feature = "postgres")]
    #[arg(long)]
    pub db_host: Option<String>,

    /// PostgreSQL port
    #[cfg(feature = "postgres")]
    #[arg(long)]
    pub db_port: Option<u16>,

    /// PostgreSQL user. The password comes from KUIPER_DATABASE__PASSWORD, or is asked for
    #[cfg(feature = "postgres")]
    #[arg(long)]
    pub db_user: Option<String>,

    /// PostgreSQL database
    #[cfg(feature = "postgres")]
    #[arg(long)]
    pub db_name: Option<String>,

    /// Replace an existing config without asking. It's kept as a backup
    #[arg(long)]
    pub force: bool,

    /// Don't ask anything: use defaults, and fail on what has none
    #[arg(short, long)]
    pub yes: bool,
}

/// Gets answers from the operator, if there is one.
struct Ask {
    /// None when there's nobody to ask
    theme: Option<ColorfulTheme>,
}

impl Ask {
    fn new(yes: bool) -> Self {
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        Self {
            theme: (!yes && terminal).then(ColorfulTheme::default),
        }
    }

    fn interactive(&self) -> bool {
        self.theme.is_some()
    }

    /// `given` if there is one, or the operator's answer, or `default`. Everything goes through `check`, a value
    /// from a flag has to be as good as a typed one. Without anyone to ask and no default, the error names `flag`.
    fn text(
        &self,
        given: Option<String>,
        prompt: &str,
        default: Option<&str>,
        flag: &str,
        check: impl Fn(&str) -> Result<(), String>,
    ) -> Result<String> {
        if let Some(value) = given {
            check(&value).map_err(|e| anyhow!("{flag}: {e}"))?;
            return Ok(value);
        }
        let Some(theme) = &self.theme else {
            let value = default.ok_or_else(|| anyhow!("{flag} is required"))?;
            check(value).map_err(|e| anyhow!("{flag}: {e}"))?;
            return Ok(value.to_string());
        };
        let mut input = Input::<String>::with_theme(theme).with_prompt(prompt);
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        input
            .validate_with(|value: &String| check(value.trim()))
            .interact_text()
            .map(|value| value.trim().to_string())
            .context("Failed to read answer")
    }

    fn confirm(&self, prompt: &str, default: bool) -> Result<bool> {
        match &self.theme {
            Some(theme) => Confirm::with_theme(theme)
                .with_prompt(prompt)
                .default(default)
                .interact()
                .context("Failed to read answer"),
            None => Ok(default),
        }
    }

    /// Index into `items`. Only for when there's someone to ask, callers work out the answer otherwise.
    fn select(&self, theme: &ColorfulTheme, prompt: &str, items: &[&str]) -> Result<usize> {
        Select::with_theme(theme)
            .with_prompt(prompt)
            .items(items)
            .default(0)
            .interact()
            .context("Failed to read answer")
    }

    /// A new password from `env`, or asked for twice. Empty from either doesn't count.
    fn password(&self, env: &str, prompt: &str) -> Result<String> {
        if let Some(password) = std::env::var(env).ok().filter(|p| !p.is_empty()) {
            return Ok(password);
        }
        let Some(theme) = &self.theme else {
            bail!("{env} has to be set, there's nobody to ask for it");
        };
        loop {
            let password = read_secret(theme, prompt)?;
            if password == read_secret(theme, "Confirm")? {
                return Ok(password);
            }
            println!("Passwords don't match");
        }
    }
}

/// What a byte typed at a secret prompt did, see [`SecretLine`].
#[derive(Debug, PartialEq, Eq)]
enum Typed {
    /// Show a `*`
    Added,
    /// Take back a `*`
    Removed,
    /// Take back this many
    Cleared(usize),
    Enter,
    Cancel,
    Ignored,
}

/// The line being typed at a secret prompt, a byte at a time.
#[derive(Default)]
struct SecretLine {
    bytes: Vec<u8>,
    /// Inside an escape sequence, like an arrow key
    escape: bool,
}

impl SecretLine {
    fn chars(&self) -> usize {
        String::from_utf8_lossy(&self.bytes).chars().count()
    }

    fn feed(&mut self, byte: u8) -> Typed {
        if self.escape {
            // ends at a letter or `~`, `\x1b[A` and `\x1b[3~` both do
            self.escape = byte == b'[' || !(byte.is_ascii_alphabetic() || byte == b'~');
            return Typed::Ignored;
        }
        match byte {
            b'\r' | b'\n' => Typed::Enter,
            // ctrl-c, ctrl-d
            0x03 | 0x04 => Typed::Cancel,
            0x1b => {
                self.escape = true;
                Typed::Ignored
            }
            // backspace: a whole character, which can be several bytes
            0x7f | 0x08 if self.bytes.is_empty() => Typed::Ignored,
            0x7f | 0x08 => {
                while self.bytes.pop().is_some_and(|b| b & 0xC0 == 0x80) {}
                Typed::Removed
            }
            // ctrl-u
            0x15 => {
                let chars = self.chars();
                self.bytes.clear();
                Typed::Cleared(chars)
            }
            0..0x20 => Typed::Ignored,
            byte => {
                self.bytes.push(byte);
                if byte & 0xC0 == 0x80 {
                    // the rest of a character that already has its `*`
                    Typed::Ignored
                } else {
                    Typed::Added
                }
            }
        }
    }
}

/// Keeps terminal echo off for as long as it lives. `console`'s key reader turns it back on between keys, which can
/// show part of a paste, and a dropped guard can't leave the shell not echoing.
#[cfg(unix)]
struct NoEcho {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl NoEcho {
    fn new(fd: std::os::fd::RawFd) -> Result<Self> {
        // SAFETY: tcgetattr fills in the zeroed struct, and tcsetattr only reads it. Both fail cleanly on a bad fd
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return Err(std::io::Error::last_os_error()).context("Failed to read terminal");
            }
            let mut quiet = original;
            // ISIG too: ctrl-c comes through as a byte, so this always gets dropped
            quiet.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
            quiet.c_cc[libc::VMIN] = 1;
            quiet.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &quiet) != 0 {
                return Err(std::io::Error::last_os_error()).context("Failed to set up terminal");
            }
            Ok(Self { fd, original })
        }
    }
}

#[cfg(unix)]
impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: `original` is what tcgetattr gave us
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// One line from the terminal, drawing a `*` for each character. A prompt that shows nothing reads like it's stuck,
/// most of all after a paste.
#[cfg(unix)]
fn read_masked(theme: &ColorfulTheme, prompt: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let mut label = String::new();
    theme
        .format_password_prompt(&mut label, prompt)
        .context("Failed to draw prompt")?;
    let mut stdout = std::io::stdout();
    write!(stdout, "{label}")?;
    stdout.flush()?;

    let stdin = std::io::stdin();
    let _no_echo = NoEcho::new(stdin.as_raw_fd())?;
    let mut line = SecretLine::default();
    let mut input = stdin.lock();
    let mut buffer = [0u8; 1024];
    loop {
        let read = input.read(&mut buffer).context("Failed to read answer")?;
        if read == 0 {
            bail!("Input ended before the answer");
        }
        for byte in &buffer[..read] {
            match line.feed(*byte) {
                Typed::Added => write!(stdout, "*")?,
                Typed::Removed => write!(stdout, "\x08 \x08")?,
                Typed::Cleared(chars) => write!(stdout, "{}", "\x08 \x08".repeat(chars))?,
                Typed::Enter => {
                    writeln!(stdout)?;
                    return String::from_utf8(line.bytes).context("Answer isn't valid text");
                }
                Typed::Cancel => {
                    writeln!(stdout)?;
                    bail!("Cancelled");
                }
                Typed::Ignored => {}
            }
        }
        stdout.flush()?;
    }
}

#[cfg(not(unix))]
fn read_masked(theme: &ColorfulTheme, prompt: &str) -> Result<String> {
    Password::with_theme(theme)
        .with_prompt(prompt)
        .interact()
        .context("Failed to read answer")
}

/// A secret that isn't empty, from the operator.
fn read_secret(theme: &ColorfulTheme, prompt: &str) -> Result<String> {
    loop {
        let secret = read_masked(theme, prompt)?;
        let secret = secret.trim();
        if !secret.is_empty() {
            return Ok(secret.to_string());
        }
        println!("It can't be empty");
    }
}

fn not_empty(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("can't be empty".to_string())
    } else {
        Ok(())
    }
}

/// Fit for a certificate name: an IP, or a DNS name.
fn check_hostname(value: &str) -> Result<(), String> {
    if value.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    let valid = !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("'{value}' isn't a hostname or IP address"))
    }
}

fn check_forgejo_url(value: &str) -> Result<(), String> {
    match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(()),
        _ => Err(format!("'{value}' isn't an http(s) URL")),
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn check_forgejo_scopes(value: &str) -> Result<(), String> {
    let forgejo = ForgejoConfig {
        url: String::new(),
        token: None,
        token_path: None,
        token_env: None,
        scopes: split_list(value),
    };
    forgejo
        .runner_scopes()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `org` or `owner/repo`.
fn parse_runner_scope(value: &str) -> Result<RunnerScope, String> {
    match value.split('/').collect::<Vec<_>>()[..] {
        [name] if !name.is_empty() => Ok(RunnerScope::Organization {
            name: name.to_string(),
        }),
        [owner, repo] if !owner.is_empty() && !repo.is_empty() => Ok(RunnerScope::Repository {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }),
        _ => Err(format!("'{value}' isn't an organization or owner/repo")),
    }
}

/// An absolute path: prompts don't get a shell's `~`, and the daemon may not run from here.
fn resolve_path(path: &Path) -> Result<PathBuf> {
    let path = match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    };
    std::path::absolute(&path).with_context(|| format!("Bad path {}", path.display()))
}

fn check_github_key(value: &str) -> Result<(), String> {
    let path = resolve_path(Path::new(value)).map_err(|e| e.to_string())?;
    let key = std::fs::read_to_string(&path)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    if key.contains("PRIVATE KEY") {
        Ok(())
    } else {
        Err(format!("{} isn't a PEM private key", path.display()))
    }
}

/// The URL agents reach `hostname` at, on `listen`'s port.
fn coordinator_url(hostname: &str, listen: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or("9443");
    if hostname.contains(':') {
        format!("https://[{hostname}]:{port}")
    } else {
        format!("https://{hostname}:{port}")
    }
}

fn hostname_step(ask: &Ask, args: &SetupArgs) -> Result<String> {
    let default = gethostname::gethostname().to_string_lossy().into_owned();
    let default = if check_hostname(&default).is_ok() {
        default
    } else {
        "localhost".to_string()
    };
    ask.text(
        args.hostname.clone(),
        "Hostname or IP agents will connect to",
        Some(&default),
        "--hostname",
        check_hostname,
    )
}

fn forge_choice(ask: &Ask, args: &SetupArgs) -> Result<ForgeChoice> {
    if let Some(forge) = args.forge {
        return Ok(forge);
    }
    let github = args.github_app_id.is_some() || args.github_key.is_some();
    let forgejo = args.forgejo_url.is_some()
        || args.forgejo_token_env.is_some()
        || args.forgejo_token_path.is_some()
        || !args.forgejo_scopes.is_empty();
    match (github, forgejo, &ask.theme) {
        (true, true, _) => bail!("Pick one of GitHub and Forgejo, setup configures one forge"),
        (true, false, _) => Ok(ForgeChoice::Github),
        (false, true, _) => Ok(ForgeChoice::Forgejo),
        (false, false, None) => Ok(ForgeChoice::None),
        (false, false, Some(theme)) => {
            let choices = [ForgeChoice::Github, ForgeChoice::Forgejo, ForgeChoice::None];
            let picked = ask.select(
                theme,
                "Which forge?",
                &[
                    "GitHub (a GitHub App)",
                    "Forgejo (15 or newer)",
                    "None yet (try it out with serve --dry-run)",
                ],
            )?;
            Ok(choices[picked])
        }
    }
}

fn github_step(ask: &Ask, args: &SetupArgs) -> Result<ForgeConfig> {
    let app_id = ask
        .text(
            args.github_app_id.map(|id| id.to_string()),
            "GitHub App ID",
            None,
            "--github-app-id",
            |value| {
                value
                    .parse::<u64>()
                    .map(|_| ())
                    .map_err(|_| format!("'{value}' isn't an App ID"))
            },
        )?
        .parse()?;
    let key = ask.text(
        args.github_key.as_ref().map(|p| p.display().to_string()),
        "Path to the App's private key (PEM)",
        None,
        "--github-key",
        check_github_key,
    )?;
    Ok(ForgeConfig {
        name: "github".to_string(),
        kind: ForgeKindConfig::Github(GitHubConfig {
            app_id,
            private_key_path: Some(resolve_path(Path::new(&key))?),
            private_key: None,
        }),
    })
}

/// (`token_env`, `token_path`) for the Forgejo config.
fn forgejo_token_step(
    ask: &Ask,
    args: &SetupArgs,
    data_dir: &Path,
) -> Result<(Option<String>, Option<PathBuf>)> {
    match (&args.forgejo_token_env, &args.forgejo_token_path) {
        (Some(_), Some(_)) => bail!("Use one of --forgejo-token-env and --forgejo-token-path"),
        (Some(env), None) => return Ok((Some(env.clone()), None)),
        (None, Some(path)) => {
            let path = resolve_path(path)?;
            std::fs::read_to_string(&path)
                .with_context(|| format!("Can't read Forgejo token {}", path.display()))?;
            return Ok((None, Some(path)));
        }
        (None, None) => {}
    }
    let Some(theme) = &ask.theme else {
        bail!("--forgejo-token-env or --forgejo-token-path is required");
    };

    let token_file = data_dir.join("forgejo-token");
    let paste = format!("Paste it (kept in {})", token_file.display());
    match ask.select(
        theme,
        "Forgejo access token",
        &[&paste, "From an environment variable", "From a file"],
    )? {
        0 => {
            let token = read_secret(theme, "Token")?;
            write_private_key(&token_file, &token)?;
            println!("✓ Saved the token to {}", token_file.display());
            Ok((None, Some(token_file)))
        }
        1 => Ok((
            Some(ask.text(
                None,
                "Variable name",
                None,
                "--forgejo-token-env",
                not_empty,
            )?),
            None,
        )),
        _ => {
            let path = ask.text(None, "Token file", None, "--forgejo-token-path", |value| {
                let path = resolve_path(Path::new(value)).map_err(|e| e.to_string())?;
                std::fs::read_to_string(&path)
                    .map(|_| ())
                    .map_err(|e| format!("can't read {}: {e}", path.display()))
            })?;
            Ok((None, Some(resolve_path(Path::new(&path))?)))
        }
    }
}

fn forgejo_step(ask: &Ask, args: &SetupArgs, data_dir: &Path) -> Result<ForgeConfig> {
    let url = ask.text(
        args.forgejo_url.clone(),
        "Forgejo URL",
        None,
        "--forgejo-url",
        check_forgejo_url,
    )?;
    let (token_env, token_path) = forgejo_token_step(ask, args, data_dir)?;
    let scopes = ask.text(
        (!args.forgejo_scopes.is_empty()).then(|| args.forgejo_scopes.join(",")),
        "Scopes to poll (org, owner/repo, @user or *, comma separated)",
        None,
        "--forgejo-scope",
        check_forgejo_scopes,
    )?;
    Ok(ForgeConfig {
        name: "forgejo".to_string(),
        kind: ForgeKindConfig::Forgejo(ForgejoConfig {
            url,
            token: None,
            token_path,
            token_env,
            scopes: split_list(&scopes),
        }),
    })
}

fn mode_step(ask: &Ask, args: &SetupArgs, forge: Option<&ForgeConfig>) -> Result<ModeChoice> {
    let github = matches!(
        forge,
        Some(ForgeConfig {
            kind: ForgeKindConfig::Github(_),
            ..
        })
    );
    let mode = args.mode.or(args
        .runner_scope
        .as_ref()
        .map(|_| ModeChoice::FixedCapacity));
    match (mode, forge, &ask.theme) {
        (Some(mode), Some(_), _) if !github && mode != ModeChoice::Polling => {
            bail!("Forgejo only supports polling, leave out --mode and --runner-scope")
        }
        (Some(mode), _, _) => Ok(mode),
        // a forge can be added later without changing this, and it's all Forgejo can do
        (None, _, Some(theme)) if github => {
            let choices = [
                ModeChoice::Polling,
                ModeChoice::Webhook,
                ModeChoice::FixedCapacity,
            ];
            let picked = ask.select(
                theme,
                "How should runners get created?",
                &[
                    "Polling: scan for queued jobs, nothing has to reach this host",
                    "Webhook: GitHub tells us about jobs, needs to reach this host",
                    "Fixed capacity: keep a pool of runners ready",
                ],
            )?;
            Ok(choices[picked])
        }
        (None, _, _) => Ok(ModeChoice::Polling),
    }
}

/// The provisioning section and fixed-capacity scope for `mode`, plus the webhook secret if it made one.
fn provisioning(
    mode: ModeChoice,
    runner_scope: Option<RunnerScope>,
) -> (ProvisioningConfig, Option<RunnerScope>, Option<String>) {
    let mut config = ProvisioningConfig::default();
    match mode {
        ModeChoice::FixedCapacity => {
            config.mode = ProvisioningMode::FixedCapacity;
            (config, runner_scope, None)
        }
        ModeChoice::Polling => {
            config.mode = ProvisioningMode::Polling;
            config.polling = Some(PollingConfig::default());
            (config, None, None)
        }
        ModeChoice::Webhook => {
            let mut secret = [0u8; 32];
            rand::rng().fill_bytes(&mut secret);
            let secret = hex::encode(secret);
            config.mode = ProvisioningMode::Webhook;
            config.webhook = Some(WebhookConfig {
                path: "/webhook".to_string(),
                secret: secret.clone(),
                required_labels: vec!["self-hosted".to_string()],
                label_mappings: Vec::new(),
                queued_scan_lookback_hours: 168,
                forge: None,
            });
            (config, None, Some(secret))
        }
    }
}

#[cfg(feature = "sqlite")]
fn database_step(_ask: &Ask, _args: &SetupArgs) -> Result<DatabaseConfig> {
    Ok(DatabaseConfig::default())
}

#[cfg(feature = "postgres")]
fn database_step(ask: &Ask, args: &SetupArgs) -> Result<DatabaseConfig> {
    let defaults = DatabaseConfig::default();
    let host = ask.text(
        args.db_host.clone(),
        "PostgreSQL host",
        Some(&defaults.host),
        "--db-host",
        not_empty,
    )?;
    let port = ask
        .text(
            args.db_port.map(|port| port.to_string()),
            "PostgreSQL port",
            Some(&defaults.port.to_string()),
            "--db-port",
            |value| {
                value
                    .parse::<u16>()
                    .map(|_| ())
                    .map_err(|_| format!("'{value}' isn't a port"))
            },
        )?
        .parse()?;
    let user = ask.text(
        args.db_user.clone(),
        "PostgreSQL user",
        None,
        "--db-user",
        not_empty,
    )?;
    let database = ask.text(
        args.db_name.clone(),
        "PostgreSQL database",
        Some(&defaults.database),
        "--db-name",
        not_empty,
    )?;
    let password = ask.password("KUIPER_DATABASE__PASSWORD", "PostgreSQL password")?;
    Ok(DatabaseConfig {
        host,
        port,
        user,
        password,
        database,
    })
}

/// Whether the server certificate comes from somewhere else, like a public CA, rather than our own CA. Agents don't
/// trust our CA for it, so replacing it with one of ours would lock them out.
fn managed_elsewhere(tls: &TlsConfig) -> bool {
    tls.server_use_native_roots
        || matches!(tls.server_trust_mode, ServerTrustMode::Chain)
        || tls
            .server_ca_cert
            .as_ref()
            .is_some_and(|ca| *ca != tls.ca_cert)
}

/// Create what's missing: an existing CA stays, and a server certificate only goes if it can't serve `hostname`.
fn certificates_step(tls: &TlsConfig, ask: &Ask, args: &SetupArgs, hostname: &str) -> Result<()> {
    if tls.ca_cert.exists() && tls.ca_key.exists() {
        println!("✓ Using the existing CA at {}", tls.ca_cert.display());
    } else {
        let org = ask.text(
            args.org.clone(),
            "Organization name for the CA certificate",
            Some(DEFAULT_ORG),
            "--org",
            not_empty,
        )?;
        init_ca(&tls.ca_cert, &tls.ca_key, &org)?;
        println!("✓ Created a CA at {}", tls.ca_cert.display());
    }

    let Some(problem) = server_identity_problem(tls, hostname) else {
        println!("✓ Using the existing server certificate for {hostname}");
        return Ok(());
    };
    if managed_elsewhere(tls) {
        println!("⚠ Agents can't use the server certificate: {problem}");
        println!(
            "  It doesn't come from our CA so setup leaves it alone, replace it before they connect"
        );
        return Ok(());
    }
    if tls.server_cert.exists() {
        println!("⚠ Replacing the server certificate: {problem}");
    }
    generate_server_cert(
        &tls.ca_cert,
        &tls.ca_key,
        &tls.server_cert,
        &tls.server_key,
        hostname,
    )?;
    // a certificate that's no good from a CA whose key doesn't match it, say, isn't a success
    if let Some(problem) = server_identity_problem(tls, hostname) {
        bail!("The new server certificate doesn't work either: {problem}");
    }
    println!("✓ Created a server certificate for {hostname}");
    Ok(())
}

/// The name to check a kept config's server certificate against: what it was issued for, so a new `--hostname` from
/// an earlier run sticks, then what agents are told to connect to, and otherwise ask.
fn kept_hostname(ask: &Ask, args: &SetupArgs, config: &Config) -> Result<String> {
    if args.hostname.is_none() {
        let from_url = (config.admin.coordinator_url != AdminConfig::default().coordinator_url)
            .then(|| url_host(&config.admin.coordinator_url))
            .flatten();
        let known = server_cert_name(&config.tls.server_cert)
            .or(from_url)
            .filter(|name| check_hostname(name).is_ok());
        if let Some(name) = known {
            return Ok(name);
        }
    }
    hostname_step(ask, args)
}

fn url_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(host.trim_matches(['[', ']']).to_string())
}

/// Whether to leave a config that's already there alone.
fn keep_config(ask: &Ask, args: &SetupArgs, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if args.force {
        return Ok(false);
    }
    println!("Found a config at {}", path.display());
    Ok(!ask.confirm("Replace it? (the old one is kept as a backup)", false)?)
}

fn save_config(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if path.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup = path.with_added_extension(format!("{stamp}.bak"));
        std::fs::rename(path, &backup)
            .with_context(|| format!("Failed to back up {}", path.display()))?;
        println!("✓ Kept the old config as {}", backup.display());
    }
    let toml = toml::to_string_pretty(config).context("Failed to serialize config")?;
    // 0600: it can hold a webhook secret or database password
    write_private_key(
        path,
        &format!(
            "# Written by `kuiper-forge setup`. examples/coordinator-config.toml has every option\n\n{toml}"
        ),
    )?;
    println!("✓ Saved the config to {}", path.display());
    Ok(())
}

/// Ask for a whole new config.
fn new_config(
    ask: &Ask,
    args: &SetupArgs,
    data_dir: &Path,
    hostname: &str,
) -> Result<(Config, Option<String>)> {
    let listen = ask.text(
        args.listen.map(|addr| addr.to_string()),
        "Address to listen on",
        Some(DEFAULT_LISTEN),
        "--listen",
        |value| {
            value
                .parse::<std::net::SocketAddr>()
                .map(|_| ())
                .map_err(|_| format!("'{value}' isn't an address like {DEFAULT_LISTEN}"))
        },
    )?;

    if ask.interactive() {
        println!("\n== Forge ==");
    }
    let forge = match forge_choice(ask, args)? {
        ForgeChoice::Github => Some(github_step(ask, args)?),
        ForgeChoice::Forgejo => Some(forgejo_step(ask, args, data_dir)?),
        ForgeChoice::None => None,
    };
    let mode = mode_step(ask, args, forge.as_ref())?;
    let runner_scope = if mode == ModeChoice::FixedCapacity {
        let scope = ask.text(
            args.runner_scope.clone(),
            "Where should runners register? (an organization or owner/repo)",
            None,
            "--runner-scope",
            |value| parse_runner_scope(value).map(|_| ()),
        )?;
        Some(parse_runner_scope(&scope).map_err(|e| anyhow!(e))?)
    } else {
        None
    };
    let (provisioning, runner_scope, webhook_secret) = provisioning(mode, runner_scope);

    let enable_admin = !args.no_admin
        && (args.admin_user.is_some()
            || (ask.interactive() && ask.confirm("Enable the admin web UI?", true)?));
    let database = database_step(ask, args)?;

    let config = Config {
        github: None,
        forges: forge.into_iter().collect(),
        grpc: GrpcConfig {
            listen_addr: listen.clone(),
            proxy_protocol: false,
        },
        tls: TlsConfig::with_defaults(data_dir),
        database,
        admin: AdminConfig {
            enabled: enable_admin,
            coordinator_url: coordinator_url(hostname, &listen),
            ..AdminConfig::default()
        },
        metrics: MetricsConfig::default(),
        provisioning,
        runner_scope,
        runner_group: None,
    };
    if !config.forges.is_empty() {
        config.resolved_forges()?;
    }
    Ok((config, webhook_secret))
}

/// Add the admin user if there isn't one by that name. Last, so nothing else is lost if it fails.
async fn admin_step(ask: &Ask, args: &SetupArgs, config: &Config, data_dir: &Path) -> Result<()> {
    if !config.admin.enabled {
        if args.admin_user.is_some() {
            println!("⚠ The admin UI is off in this config, set [admin] enabled = true to use it");
        }
        return Ok(());
    }

    let db = Database::new(&config.database, data_dir).await?;
    let admins = AdminAuthStore::new(db.pool());
    if args.admin_user.is_none() && !admins.list_users().await?.is_empty() {
        println!("✓ There's already an admin user");
        return Ok(());
    }
    let username = ask.text(
        args.admin_user.clone(),
        "Admin username",
        Some(DEFAULT_ADMIN_USER),
        "--admin-user",
        not_empty,
    )?;
    if admins.get_user(&username).await?.is_some() {
        println!("✓ Admin user '{username}' already exists");
        return Ok(());
    }
    let password = ask.password(ADMIN_PASSWORD_ENV, "Admin password")?;
    if password.is_empty() {
        bail!("The admin password can't be empty");
    }
    admins.create_user(&username, &password).await?;
    println!("✓ Created admin user '{username}'");
    Ok(())
}

/// Print what to do next. Paths are spelled out when they aren't the defaults, so the commands work as pasted.
fn next_steps(
    config: &Config,
    config_path: &Path,
    data_dir: &Path,
    url: &str,
    webhook: Option<&str>,
) {
    let mut bin = "kuiper-forge".to_string();
    if config_path != Config::default_path() {
        bin.push_str(&format!(" -c {}", config_path.display()));
    }
    if data_dir != Config::default_data_dir() {
        bin.push_str(&format!(" -d {}", data_dir.display()));
    }

    println!("\n== Next ==");
    if config.forges.is_empty() && config.github.is_none() {
        println!("Start it without a forge:  {bin} serve --dry-run");
        println!("Add a [[forges]] entry to the config to serve real jobs");
    } else {
        println!("Start it:          {bin} serve");
    }
    println!("Register an agent: {bin} token create --url {url}");
    if config.admin.enabled {
        println!("Admin UI:          {url}/admin");
    }
    if let Some(secret) = webhook {
        println!(
            "\nPoint your GitHub App's webhook at {url}/webhook, subscribed to Workflow jobs,"
        );
        println!("with this secret (it's in the config too): {secret}");
    }
}

/// Run setup, see the [module docs](self).
pub async fn run(args: SetupArgs, config_path: &Path, data_dir: &Path) -> Result<()> {
    let ask = Ask::new(args.yes);
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Failed to create {}", data_dir.display()))?;

    if ask.interactive() {
        println!("== Coordinator ==");
    }
    let keep = keep_config(&ask, &args, config_path)?;
    let (config, webhook_secret, hostname) = if keep {
        let config = Config::load(config_path)?;
        let hostname = kept_hostname(&ask, &args, &config)?;
        (config, None, hostname)
    } else {
        let hostname = hostname_step(&ask, &args)?;
        let (config, secret) = new_config(&ask, &args, data_dir, &hostname)?;
        (config, secret, hostname)
    };

    println!("\n== Certificates ==");
    certificates_step(&config.tls, &ask, &args, &hostname)?;
    if !keep {
        save_config(&config, config_path)?;
    }

    println!("\n== Admin ==");
    admin_step(&ask, &args, &config, data_dir).await?;

    // agents need a URL the server certificate covers, a kept config can be behind a new --hostname
    let url = match &args.hostname {
        Some(hostname) => coordinator_url(hostname, &config.grpc.listen_addr),
        None => config.admin.coordinator_url.clone(),
    };
    if url != config.admin.coordinator_url {
        println!(
            "\n⚠ [admin] coordinator_url in the config is still {}, change it to {url}",
            config.admin.coordinator_url
        );
    } else if let Some(host) = url_host(&url)
        && server_identity_problem(&config.tls, &host).is_some()
        && !managed_elsewhere(&config.tls)
    {
        println!(
            "\n⚠ [admin] coordinator_url is {url}, but the server certificate is for {hostname}"
        );
    }
    next_steps(
        &config,
        config_path,
        data_dir,
        &url,
        webhook_secret.as_deref(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn any_value(_: &str) -> Result<(), String> {
        Ok(())
    }

    fn nobody() -> Ask {
        Ask { theme: None }
    }

    /// What ends up typed, and how many `*` are on screen, after feeding `input`.
    fn typed(input: &[u8]) -> (String, usize, Option<Typed>) {
        let mut line = SecretLine::default();
        let mut stars = 0usize;
        for byte in input {
            match line.feed(*byte) {
                Typed::Added => stars += 1,
                Typed::Removed => stars -= 1,
                Typed::Cleared(chars) => stars -= chars,
                Typed::Ignored => {}
                end => {
                    return (String::from_utf8(line.bytes).unwrap(), stars, Some(end));
                }
            }
        }
        (String::from_utf8(line.bytes).unwrap(), stars, None)
    }

    #[test]
    fn test_secret_line() {
        assert_eq!(typed(b"tok\r"), ("tok".into(), 3, Some(Typed::Enter)));
        assert_eq!(typed(b"tok\n"), ("tok".into(), 3, Some(Typed::Enter)));
        assert_eq!(typed(b"to\x03"), ("to".into(), 2, Some(Typed::Cancel)));
        assert_eq!(typed(b"to\x04"), ("to".into(), 2, Some(Typed::Cancel)));
    }

    #[test]
    fn test_secret_line_editing() {
        assert_eq!(typed(b"tox\x7fk\r").0, "tok");
        assert_eq!(typed(b"tox\x08k\r").0, "tok");
        assert_eq!(typed(b"\x7f\x7fa").1, 1);
        assert_eq!(typed(b"abc\x15de\r"), ("de".into(), 2, Some(Typed::Enter)));
    }

    #[test]
    fn test_secret_line_ignores_keys_that_arent_text() {
        // arrow, delete, tab
        assert_eq!(typed(b"a\x1b[Ab\x1b[3~c\td\r").0, "abcd");
    }

    #[test]
    fn test_secret_line_counts_characters_not_bytes() {
        assert_eq!(typed("é€x".as_bytes()).1, 3);
        let (text, stars, _) = typed("é€\x7fx\r".as_bytes());
        assert_eq!((text.as_str(), stars), ("éx", 2));
        assert_eq!(typed("é€\x15".as_bytes()).1, 0);
    }

    #[test]
    fn test_text_prefers_the_flag_then_the_default() {
        let ask = nobody();
        let given = ask.text(Some("a".into()), "?", Some("b"), "--x", any_value);
        assert_eq!(given.unwrap(), "a");
        let default = ask.text(None, "?", Some("b"), "--x", any_value);
        assert_eq!(default.unwrap(), "b");
    }

    #[test]
    fn test_text_without_an_answer_names_the_flag() {
        let err = nobody()
            .text(None, "?", None, "--github-app-id", any_value)
            .unwrap_err();
        assert!(err.to_string().contains("--github-app-id"));
    }

    #[test]
    fn test_text_checks_flags_like_answers() {
        let err = nobody()
            .text(
                Some("not a host".into()),
                "?",
                None,
                "--hostname",
                check_hostname,
            )
            .unwrap_err();
        assert!(err.to_string().contains("--hostname"));
    }

    #[test]
    fn test_confirm_without_anyone_takes_the_default() {
        assert!(nobody().confirm("?", true).unwrap());
        assert!(!nobody().confirm("?", false).unwrap());
    }

    #[test]
    fn test_check_hostname() {
        for good in [
            "localhost",
            "ci.example.com",
            "mac-mini_1",
            "10.0.0.5",
            "::1",
            "fe80::1",
        ] {
            assert!(check_hostname(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "not a host",
            "a..b",
            "-a.com",
            "a-.com",
            "a/b",
            "*.x.com",
        ] {
            assert!(check_hostname(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_parse_runner_scope() {
        assert_eq!(
            parse_runner_scope("astro"),
            Ok(RunnerScope::Organization {
                name: "astro".into()
            })
        );
        assert_eq!(
            parse_runner_scope("astro/app"),
            Ok(RunnerScope::Repository {
                owner: "astro".into(),
                repo: "app".into()
            })
        );
        for bad in ["", "/", "a/", "/b", "a/b/c"] {
            assert!(parse_runner_scope(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_coordinator_url() {
        assert_eq!(
            coordinator_url("ci.local", "0.0.0.0:9443"),
            "https://ci.local:9443"
        );
        assert_eq!(
            coordinator_url("10.0.0.5", "[::]:8443"),
            "https://10.0.0.5:8443"
        );
        assert_eq!(
            coordinator_url("fe80::1", "[::]:9443"),
            "https://[fe80::1]:9443"
        );
    }

    #[test]
    fn test_resolve_path() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_path(Path::new("~/key.pem")).unwrap(),
            home.join("key.pem")
        );
        assert_eq!(
            resolve_path(Path::new("/etc/k")).unwrap(),
            Path::new("/etc/k")
        );
        assert!(resolve_path(Path::new("rel/k")).unwrap().is_absolute());
    }

    #[test]
    fn test_github_key_must_be_a_private_key() {
        let dir = TempDir::new().unwrap();
        let key = dir.path().join("key.pem");
        assert!(check_github_key(key.to_str().unwrap()).is_err());
        std::fs::write(&key, "hello").unwrap();
        assert!(check_github_key(key.to_str().unwrap()).is_err());
        std::fs::write(&key, "-----BEGIN RSA PRIVATE KEY-----\n").unwrap();
        assert!(check_github_key(key.to_str().unwrap()).is_ok());
    }

    #[test]
    fn test_forge_is_implied_by_its_flags() {
        let ask = nobody();
        let github = SetupArgs {
            github_app_id: Some(1),
            ..Default::default()
        };
        assert_eq!(forge_choice(&ask, &github).unwrap(), ForgeChoice::Github);
        let forgejo = SetupArgs {
            forgejo_url: Some("https://git.example.com".into()),
            ..Default::default()
        };
        assert_eq!(forge_choice(&ask, &forgejo).unwrap(), ForgeChoice::Forgejo);
        assert_eq!(
            forge_choice(&ask, &SetupArgs::default()).unwrap(),
            ForgeChoice::None
        );
        let both = SetupArgs {
            github_app_id: Some(1),
            forgejo_url: Some("https://git.example.com".into()),
            ..Default::default()
        };
        assert!(forge_choice(&ask, &both).is_err());
    }

    fn github() -> ForgeConfig {
        ForgeConfig {
            name: "github".into(),
            kind: ForgeKindConfig::Github(GitHubConfig {
                app_id: 1,
                private_key_path: Some("/k.pem".into()),
                private_key: None,
            }),
        }
    }

    fn forgejo() -> ForgeConfig {
        ForgeConfig {
            name: "forgejo".into(),
            kind: ForgeKindConfig::Forgejo(ForgejoConfig {
                url: "https://git.example.com".into(),
                token: None,
                token_path: None,
                token_env: Some("TOKEN".into()),
                scopes: vec!["astro".into()],
            }),
        }
    }

    #[test]
    fn test_mode() {
        let ask = nobody();
        let args = SetupArgs::default();
        assert_eq!(
            mode_step(&ask, &args, Some(&github())).unwrap(),
            ModeChoice::Polling
        );
        let scoped = SetupArgs {
            runner_scope: Some("astro".into()),
            ..Default::default()
        };
        assert_eq!(
            mode_step(&ask, &scoped, Some(&github())).unwrap(),
            ModeChoice::FixedCapacity
        );
        assert!(mode_step(&ask, &scoped, Some(&forgejo())).is_err());
        assert_eq!(
            mode_step(&ask, &args, Some(&forgejo())).unwrap(),
            ModeChoice::Polling
        );
    }

    /// Every combination setup can write has to load and pass validation.
    #[test]
    fn test_saved_configs_load() {
        let scope = RunnerScope::Organization {
            name: "astro".into(),
        };
        let cases = [
            (Some(github()), ModeChoice::Polling, None),
            (Some(github()), ModeChoice::Webhook, None),
            (Some(github()), ModeChoice::FixedCapacity, Some(scope)),
            (Some(forgejo()), ModeChoice::Polling, None),
            (None, ModeChoice::Polling, None),
        ];
        for (forge, mode, scope) in cases {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("sub/config.toml");
            let (provisioning, runner_scope, secret) = provisioning(mode, scope);
            assert_eq!(secret.is_some(), mode == ModeChoice::Webhook);
            let has_forge = forge.is_some();
            let config = Config {
                github: None,
                forges: forge.into_iter().collect(),
                grpc: GrpcConfig::default(),
                tls: TlsConfig::with_defaults(dir.path()),
                database: DatabaseConfig::default(),
                admin: AdminConfig::default(),
                metrics: MetricsConfig::default(),
                provisioning,
                runner_scope,
                runner_group: None,
            };
            save_config(&config, &path).unwrap();

            let loaded = Config::load(&path).unwrap();
            assert_eq!(loaded.provisioning.mode, config.provisioning.mode);
            assert_eq!(loaded.tls.ca_cert, config.tls.ca_cert);
            if has_forge {
                assert_eq!(loaded.resolved_forges().unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn test_webhook_secrets_differ() {
        let secret = || provisioning(ModeChoice::Webhook, None).2.unwrap();
        let first = secret();
        assert_eq!(first.len(), 64);
        assert_ne!(first, secret());
    }

    #[cfg(unix)]
    #[test]
    fn test_saved_config_is_private_and_keeps_a_backup() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        let config = Config {
            github: None,
            forges: Vec::new(),
            grpc: GrpcConfig::default(),
            tls: TlsConfig::with_defaults(dir.path()),
            database: DatabaseConfig::default(),
            admin: AdminConfig::default(),
            metrics: MetricsConfig::default(),
            provisioning: ProvisioningConfig::default(),
            runner_scope: None,
            runner_group: None,
        };
        save_config(&config, &path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), "old");
    }

    fn covers(tls: &TlsConfig, hostname: &str) -> bool {
        server_identity_problem(tls, hostname).is_none()
    }

    /// A CA and a working server certificate for `hostname` in a new directory.
    fn issued(hostname: &str) -> (TempDir, TlsConfig) {
        let dir = TempDir::new().unwrap();
        let tls = TlsConfig::with_defaults(dir.path());
        certificates_step(&tls, &nobody(), &SetupArgs::default(), hostname).unwrap();
        assert!(covers(&tls, hostname));
        (dir, tls)
    }

    /// Replace the server certificate with one that expired yesterday, from the same CA.
    fn expire(tls: &TlsConfig, hostname: &str) {
        let now = ::time::OffsetDateTime::now_utc();
        crate::auth::issue_server_cert(
            &tls.ca_cert,
            &tls.ca_key,
            &tls.server_cert,
            &tls.server_key,
            hostname,
            now - ::time::Duration::days(30),
            now - ::time::Duration::days(1),
        )
        .unwrap();
    }

    #[test]
    fn test_server_cert_names() {
        let (_dir, tls) = issued("10.0.0.5");
        let generate = |hostname| {
            generate_server_cert(
                &tls.ca_cert,
                &tls.ca_key,
                &tls.server_cert,
                &tls.server_key,
                hostname,
            )
        };

        // an IP is an IP name, a DNS name with those characters wouldn't match it
        assert_eq!(server_cert_name(&tls.server_cert).unwrap(), "10.0.0.5");
        assert!(covers(&tls, "localhost"));
        assert!(!covers(&tls, "10.0.0.6"));

        generate("::1").unwrap();
        assert!(covers(&tls, "::1"));
        assert_eq!(server_cert_name(&tls.server_cert).unwrap(), "::1");

        generate("ci.local").unwrap();
        assert!(covers(&tls, "CI.Local"));
        assert!(!covers(&tls, "other.local"));
        assert_eq!(server_cert_name(&tls.server_cert).unwrap(), "ci.local");

        // an error, it used to panic
        assert!(generate("bad\u{e9}host").is_err());
    }

    #[test]
    fn test_expired_certificate_is_a_problem_and_gets_replaced() {
        let (_dir, tls) = issued("ci.local");
        expire(&tls, "ci.local");
        assert!(
            server_identity_problem(&tls, "ci.local")
                .unwrap()
                .to_lowercase()
                .contains("expired")
        );
        // what it's for can still be read, so a reissue keeps the name
        assert_eq!(server_cert_name(&tls.server_cert).unwrap(), "ci.local");

        certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
        assert!(covers(&tls, "ci.local"));
    }

    #[test]
    fn test_missing_key_gets_the_certificate_replaced() {
        let (_dir, tls) = issued("ci.local");
        std::fs::remove_file(&tls.server_key).unwrap();
        assert!(!covers(&tls, "ci.local"));

        certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
        assert!(covers(&tls, "ci.local"));
    }

    #[test]
    fn test_another_certificates_key_gets_the_certificate_replaced() {
        let (_dir, tls) = issued("ci.local");
        let (_other_dir, other) = issued("ci.local");
        std::fs::copy(&other.server_key, &tls.server_key).unwrap();
        let problem = server_identity_problem(&tls, "ci.local").unwrap();
        assert!(problem.contains("isn't the key of"), "{problem}");

        certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
        assert!(covers(&tls, "ci.local"));
    }

    #[test]
    fn test_garbled_certificate_gets_replaced() {
        let (_dir, tls) = issued("ci.local");
        std::fs::write(&tls.server_cert, "-----BEGIN CERTIFICATE-----\nnope\n").unwrap();
        assert!(!covers(&tls, "ci.local"));
        assert_eq!(server_cert_name(&tls.server_cert), None);

        certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
        assert!(covers(&tls, "ci.local"));
    }

    #[test]
    fn test_certificate_from_a_replaced_ca_gets_replaced() {
        let (_dir, tls) = issued("ci.local");
        let (_other_dir, other) = issued("ci.local");
        std::fs::copy(&other.ca_cert, &tls.ca_cert).unwrap();
        std::fs::copy(&other.ca_key, &tls.ca_key).unwrap();
        assert!(!covers(&tls, "ci.local"));

        certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
        assert!(covers(&tls, "ci.local"));
    }

    #[test]
    fn test_ca_key_that_cant_sign_for_the_ca_is_an_error() {
        let (_dir, tls) = issued("ci.local");
        let (_other_dir, other) = issued("ci.local");
        std::fs::copy(&other.ca_key, &tls.ca_key).unwrap();
        expire(&tls, "ci.local");

        // the new certificate can't verify, setup mustn't say it worked
        let err = certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local");
        assert!(err.unwrap_err().to_string().contains("doesn't work either"));
    }

    #[test]
    fn test_certificates_from_elsewhere_are_left_alone() {
        for elsewhere in [
            |tls: &mut TlsConfig| tls.server_use_native_roots = true,
            |tls: &mut TlsConfig| tls.server_trust_mode = ServerTrustMode::Chain,
            |tls: &mut TlsConfig| tls.server_ca_cert = Some("/public/bundle.pem".into()),
        ] {
            let (_dir, mut tls) = issued("ci.local");
            elsewhere(&mut tls);
            assert!(managed_elsewhere(&tls));
            expire(&tls, "ci.local");
            let cert = std::fs::read(&tls.server_cert).unwrap();

            certificates_step(&tls, &nobody(), &SetupArgs::default(), "ci.local").unwrap();
            assert_eq!(std::fs::read(&tls.server_cert).unwrap(), cert);
        }
    }

    #[test]
    fn test_server_ca_set_to_our_own_ca_isnt_elsewhere() {
        let (_dir, mut tls) = issued("ci.local");
        tls.server_ca_cert = Some(tls.ca_cert.clone());
        assert!(!managed_elsewhere(&tls));
    }

    fn kept(url: &str, tls: TlsConfig) -> Config {
        Config {
            github: None,
            forges: Vec::new(),
            grpc: GrpcConfig::default(),
            tls,
            database: DatabaseConfig::default(),
            admin: AdminConfig {
                coordinator_url: url.to_string(),
                ..AdminConfig::default()
            },
            metrics: MetricsConfig::default(),
            provisioning: ProvisioningConfig::default(),
            runner_scope: None,
            runner_group: None,
        }
    }

    #[test]
    fn test_kept_hostname() {
        let ask = nobody();
        let args = SetupArgs::default();

        // the certificate's name wins, an earlier --hostname mustn't get undone by an older URL
        let (_dir, tls) = issued("new.local");
        let config = kept("https://old.local:9443", tls);
        assert_eq!(kept_hostname(&ask, &args, &config).unwrap(), "new.local");

        // an expired one still says what it's for
        expire(&config.tls, "new.local");
        assert_eq!(kept_hostname(&ask, &args, &config).unwrap(), "new.local");

        // the flag beats both
        let flag = SetupArgs {
            hostname: Some("flag.local".into()),
            ..Default::default()
        };
        assert_eq!(kept_hostname(&ask, &flag, &config).unwrap(), "flag.local");

        // no certificate to go on: the URL, brackets off an IPv6 address
        let empty = TempDir::new().unwrap();
        let none = TlsConfig::with_defaults(empty.path());
        let config = kept("https://[fe80::1]:9443", none.clone());
        assert_eq!(kept_hostname(&ask, &args, &config).unwrap(), "fe80::1");

        // the default URL isn't anyone's choice, so this is the system's name, not localhost by accident
        let config = kept(&AdminConfig::default().coordinator_url, none);
        let name = kept_hostname(&ask, &args, &config).unwrap();
        assert_eq!(name, hostname_step(&ask, &args).unwrap());
    }

    #[test]
    fn test_certificates_step_keeps_the_ca() {
        let dir = TempDir::new().unwrap();
        let tls = TlsConfig::with_defaults(dir.path());
        let (ask, args) = (nobody(), SetupArgs::default());

        certificates_step(&tls, &ask, &args, "ci.local").unwrap();
        let ca = std::fs::read(&tls.ca_key).unwrap();
        let cert = std::fs::read(&tls.server_cert).unwrap();

        // nothing to do: same hostname
        certificates_step(&tls, &ask, &args, "ci.local").unwrap();
        assert_eq!(std::fs::read(&tls.server_cert).unwrap(), cert);

        // a new hostname needs a new server certificate, from the same CA
        certificates_step(&tls, &ask, &args, "10.0.0.5").unwrap();
        assert_ne!(std::fs::read(&tls.server_cert).unwrap(), cert);
        assert_eq!(std::fs::read(&tls.ca_key).unwrap(), ca);
        assert!(covers(&tls, "10.0.0.5"));
    }
}
