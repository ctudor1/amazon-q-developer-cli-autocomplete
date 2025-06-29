use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{
    ArgAction,
    Args,
    ValueEnum,
};
use crossterm::{
    execute,
    style,
};
use eyre::{
    Result,
    bail,
};
use tracing::warn;

use crate::cli::chat::tool_manager::{
    McpServerConfig,
    global_mcp_config_path,
    profile_mcp_path,
    workspace_mcp_config_path,
};
use crate::cli::chat::tools::custom_tool::{
    CustomToolConfig,
    default_timeout,
};
use crate::os::Os;

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    Workspace,
    Global,
    Profile,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Workspace => write!(f, "workspace"),
            Scope::Global => write!(f, "global"),
            Scope::Profile => write!(f, "profile"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum McpSubcommand {
    /// Add or replace a configured server
    Add(AddArgs),
    /// Remove a server from the MCP configuration
    #[command(alias = "rm")]
    Remove(RemoveArgs),
    /// List configured servers
    List(ListArgs),
    /// Import a server configuration from another file
    Import(ImportArgs),
    /// Get the status of a configured server
    Status(StatusArgs),
    /// Configure profile-exclusive server usage
    #[command(alias = "ab")]
    UseProfileServersOnly(UseProfileServersOnlyArgs),
}

impl McpSubcommand {
    pub async fn execute(self, os: &mut Os, output: &mut impl Write) -> Result<ExitCode> {
        match self {
            Self::Add(args) => args.execute(os, output).await?,
            Self::Remove(args) => args.execute(os, output).await?,
            Self::List(args) => args.execute(os, output).await?,
            Self::Import(args) => args.execute(os, output).await?,
            Self::Status(args) => args.execute(os, output).await?,
            Self::UseProfileServersOnly(args) => args.execute(os, output).await?,
        }

        output.flush()?;
        Ok(ExitCode::SUCCESS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AddArgs {
    /// Name for the server
    #[arg(long)]
    pub name: String,
    /// The command used to launch the server
    #[arg(long)]
    pub command: String,
    /// Arguments to pass to the command
    #[arg(long, action = ArgAction::Append, allow_hyphen_values = true, value_delimiter = ',')]
    pub args: Vec<String>,
    /// Where to add the server to.
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    /// Profile name when using profile scope
    #[arg(long)]
    pub profile: Option<String>,
    /// Environment variables to use when launching the server
    #[arg(long, value_parser = parse_env_vars)]
    pub env: Vec<HashMap<String, String>>,
    /// Server launch timeout, in milliseconds
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Whether the server should be disabled (not loaded)
    #[arg(long, default_value_t = false)]
    pub disabled: bool,
    /// Overwrite an existing server with the same name
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

impl AddArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        let scope = self.scope.unwrap_or(Scope::Workspace);
        let config_path = resolve_scope_profile(os, self.scope, self.profile.clone())?;

        let mut config: McpServerConfig = ensure_config_file(os, &config_path, output).await?;

        if config.mcp_servers.contains_key(&self.name) && !self.force {
            bail!(
                "\nMCP server '{}' already exists in {} (scope {}). Use --force to overwrite.",
                self.name,
                config_path.display(),
                scope
            );
        }

        let merged_env = self.env.into_iter().flatten().collect::<HashMap<_, _>>();
        let tool: CustomToolConfig = serde_json::from_value(serde_json::json!({
            "command": self.command,
            "args": self.args,
            "env": merged_env,
            "timeout": self.timeout.unwrap_or(default_timeout()),
            "disabled": self.disabled,
        }))?;

        writeln!(
            output,
            "\nTo learn more about MCP safety, see https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/command-line-mcp-security.html\n\n"
        )?;

        config.mcp_servers.insert(self.name.clone(), tool);
        config.save_to_file(os, &config_path).await?;
        writeln!(
            output,
            "✓ Added MCP server '{}' to {}\n",
            self.name,
            scope_display(&scope)
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RemoveArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,
    /// Profile name when using profile scope
    #[arg(long)]
    pub profile: Option<String>,
}

impl RemoveArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        let scope = self.scope.unwrap_or(Scope::Workspace);
        let config_path = resolve_scope_profile(os, self.scope, self.profile.clone())?;

        if !os.fs.exists(&config_path) {
            writeln!(output, "\nNo MCP server configurations found.\n")?;
            return Ok(());
        }

        let mut config = McpServerConfig::load_from_file(os, &config_path).await?;
        match config.mcp_servers.remove(&self.name) {
            Some(_) => {
                config.save_to_file(os, &config_path).await?;
                writeln!(
                    output,
                    "\n✓ Removed MCP server '{}' from {}\n",
                    self.name,
                    scope_display(&scope)
                )?;
            },
            None => {
                writeln!(
                    output,
                    "\nNo MCP server named '{}' found in {}\n",
                    self.name,
                    scope_display(&scope)
                )?;
            },
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ListArgs {
    #[arg(value_enum)]
    pub scope: Option<Scope>,
    #[arg(long, hide = true)]
    pub profile: Option<String>,
}

impl ListArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        let configs = get_mcp_server_configs(os, self.scope, self.profile.clone()).await?;
        if configs.is_empty() {
            writeln!(output, "No MCP server configurations found.\n")?;
            return Ok(());
        }

        // Check for profile exclusivity warning
        if let Some(profile_name) = &self.profile {
            for (scope, _path, cfg_opt) in &configs {
                if let (Scope::Profile, Some(cfg)) = (scope, cfg_opt) {
                    if cfg.use_profile_servers_only {
                        queue_profile_exclusive_warning(output, profile_name)?;
                        writeln!(output)?;
                        break;
                    }
                }
            }
        }

        for (scope, path, cfg_opt) in configs {
            writeln!(output)?;
            writeln!(output, "{}:\n  {}", scope_display(&scope), path.display())?;
            match cfg_opt {
                Some(cfg) if !cfg.mcp_servers.is_empty() => {
                    for (name, tool_cfg) in &cfg.mcp_servers {
                        let status = if tool_cfg.disabled { " (disabled)" } else { "" };
                        writeln!(output, "    • {name:<12} {}{}", tool_cfg.command, status)?;
                    }
                },
                _ => {
                    writeln!(output, "    (empty)")?;
                },
            }
        }
        writeln!(output, "\n")?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ImportArgs {
    #[arg(long)]
    pub file: String,
    #[arg(value_enum)]
    pub scope: Option<Scope>,
    /// Profile name when using profile scope
    #[arg(long)]
    pub profile: Option<String>,
    /// Overwrite an existing server with the same name
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

impl ImportArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        let scope: Scope = self.scope.unwrap_or(Scope::Workspace);
        let config_path = resolve_scope_profile(os, self.scope, self.profile.clone())?;
        let mut dst_cfg = ensure_config_file(os, &config_path, output).await?;

        let src_path = expand_path(os, &self.file)?;
        let src_cfg: McpServerConfig = McpServerConfig::load_from_file(os, &src_path).await?;

        let mut added = 0;
        for (name, cfg) in src_cfg.mcp_servers {
            if dst_cfg.mcp_servers.contains_key(&name) && !self.force {
                bail!(
                    "\nMCP server '{}' already exists in {} (scope {}). Use --force to overwrite.\n",
                    name,
                    config_path.display(),
                    scope
                );
            }
            dst_cfg.mcp_servers.insert(name.clone(), cfg);
            added += 1;
        }

        writeln!(
            output,
            "\nTo learn more about MCP safety, see https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/command-line-mcp-security.html\n\n"
        )?;

        dst_cfg.save_to_file(os, &config_path).await?;
        writeln!(
            output,
            "✓ Imported {added} MCP server(s) into {}\n",
            scope_display(&scope)
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub name: String,
    /// Profile name when using profile scope
    #[arg(long)]
    pub profile: Option<String>,
}

impl StatusArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        let configs = get_mcp_server_configs(os, None, self.profile.clone()).await?;
        let mut found = false;

        // Check for profile exclusivity warning
        if let Some(profile_name) = &self.profile {
            for (scope, _path, cfg_opt) in &configs {
                if let (Scope::Profile, Some(cfg)) = (scope, cfg_opt) {
                    if cfg.use_profile_servers_only {
                        queue_profile_exclusive_warning(output, profile_name)?;
                        writeln!(output)?;
                        break;
                    }
                }
            }
        }

        for (sc, path, cfg_opt) in configs {
            if let Some(cfg) = cfg_opt.and_then(|c| c.mcp_servers.get(&self.name).cloned()) {
                found = true;
                execute!(
                    output,
                    style::Print("\n─────────────\n"),
                    style::Print(format!("Scope   : {}\n", scope_display(&sc))),
                    style::Print(format!("File    : {}\n", path.display())),
                    style::Print(format!("Command : {}\n", cfg.command)),
                    style::Print(format!("Timeout : {} ms\n", cfg.timeout)),
                    style::Print(format!("Disabled: {}\n", cfg.disabled)),
                    style::Print(format!(
                        "Env Vars: {}\n",
                        cfg.env
                            .as_ref()
                            .map_or_else(|| "(none)".into(), |e| e.keys().cloned().collect::<Vec<_>>().join(", "))
                    )),
                )?;
            }
        }
        writeln!(output, "\n")?;

        if !found {
            bail!("No MCP server named '{}' found in any scope/profile\n", self.name);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct UseProfileServersOnlyArgs {
    /// Profile name to configure
    #[arg(long)]
    pub profile: String,
    /// Whether to use only profile servers (true) or allow inheritance (false)
    #[arg(long, value_name = "BOOLEAN", default_value = "true", action = clap::ArgAction::Set)]
    pub value: bool,
}

impl UseProfileServersOnlyArgs {
    pub async fn execute(self, os: &Os, output: &mut impl Write) -> Result<()> {
        // Get the profile MCP path
        let profile_mcp_path = profile_mcp_path(os, &self.profile)?;

        // Check if the profile exists
        let profile_dir = profile_mcp_path
            .parent()
            .ok_or_else(|| eyre::eyre!("Invalid profile path"))?;
        
        if !os.fs.exists(profile_dir) {
            bail!("Profile '{}' does not exist", self.profile);
        }

        // Load or create the profile MCP configuration
        let mut config = if os.fs.exists(&profile_mcp_path) {
            match McpServerConfig::load_from_file(os, &profile_mcp_path).await {
                Ok(config) => config,
                Err(e) => {
                    warn!("Failed to load profile MCP config: {}", e);
                    McpServerConfig::default()
                },
            }
        } else {
            McpServerConfig::default()
        };

        // Set the exclusivity flag
        config.use_profile_servers_only = self.value;

        // Save the configuration
        if let Some(parent) = profile_mcp_path.parent() {
            os.fs.create_dir_all(parent).await?;
        }
        config.save_to_file(os, &profile_mcp_path).await?;

        writeln!(
            output,
            "✓ Set profile '{}' to {} use profile-specific MCP servers exclusively\n",
            self.profile,
            if self.value { "now" } else { "no longer" }
        )?;

        Ok(())
    }
}

/// Enhanced multi-scope configuration loading with profile exclusivity support
async fn get_mcp_server_configs(
    os: &Os,
    scope: Option<Scope>,
    profile: Option<String>,
) -> Result<Vec<(Scope, PathBuf, Option<McpServerConfig>)>> {
    let mut results = Vec::new();
    
    // If a specific scope is requested, only load that scope
    if let Some(requested_scope) = scope {
        let path = resolve_scope_profile(os, Some(requested_scope), profile)?;
        let cfg_opt = load_config_with_error_handling(os, &path).await;
        results.push((requested_scope, path, cfg_opt));
        return Ok(results);
    }

    // Multi-scope loading in priority order: Profile → Workspace → Global
    let mut scopes_to_load = Vec::new();
    
    // Add Profile scope if profile name is provided
    if let Some(ref profile_name) = profile {
        scopes_to_load.push((Scope::Profile, Some(profile_name.clone())));
    }
    
    // Always add Workspace and Global scopes
    scopes_to_load.push((Scope::Workspace, None));
    scopes_to_load.push((Scope::Global, None));

    // Load configurations in order and check for profile exclusivity
    for (scope_type, profile_name) in scopes_to_load {
        let path = resolve_scope_profile(os, Some(scope_type), profile_name)?;
        let cfg_opt = load_config_with_error_handling(os, &path).await;
        
        // Check for profile exclusivity
        if let (Scope::Profile, Some(cfg)) = (&scope_type, &cfg_opt) {
            if cfg.use_profile_servers_only {
                // Profile exclusivity is enabled - only return profile configuration
                results.push((scope_type, path, cfg_opt));
                return Ok(results);
            }
        }
        
        results.push((scope_type, path, cfg_opt));
    }
    
    Ok(results)
}

/// Helper function to load configuration with consistent error handling
async fn load_config_with_error_handling(
    os: &Os,
    path: &PathBuf,
) -> Option<McpServerConfig> {
    if os.fs.exists(path) {
        match McpServerConfig::load_from_file(os, path).await {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                warn!(?path, error = %e, "Invalid MCP config file—ignored, treated as null");
                None
            },
        }
    } else {
        None
    }
}

fn scope_display(scope: &Scope) -> String {
    match scope {
        Scope::Workspace => "📄 workspace".into(),
        Scope::Global => "🌍 global".into(),
        Scope::Profile => "👤 profile".into(),
    }
}

fn resolve_scope_profile(os: &Os, scope: Option<Scope>, profile: Option<String>) -> Result<PathBuf> {
    Ok(match scope {
        Some(Scope::Global) => global_mcp_config_path(os)?,
        Some(Scope::Profile) => {
            let profile_name = profile.ok_or_else(|| eyre::eyre!("Profile name is required when using profile scope"))?;
            profile_mcp_path(os, &profile_name)?
        },
        _ => workspace_mcp_config_path(os)?,
    })
}

fn queue_profile_exclusive_warning(output: &mut impl Write, profile_name: &str) -> Result<()> {
    writeln!(
        output,
        "⚠️  Profile '{}' is configured for exclusive server usage.",
        profile_name
    )?;
    writeln!(
        output, 
        "   Only MCP servers defined in this profile will be loaded."
    )?;
    writeln!(
        output,
        "   Global and workspace servers will be ignored."
    )?;
    Ok(())
}

fn expand_path(os: &Os, p: &str) -> Result<PathBuf> {
    let p = shellexpand::tilde(p);
    let mut path = PathBuf::from(p.as_ref() as &str);
    if path.is_relative() {
        path = os.env.current_dir()?.join(path);
    }
    Ok(path)
}

async fn ensure_config_file(os: &Os, path: &PathBuf, output: &mut impl Write) -> Result<McpServerConfig> {
    if !os.fs.exists(path) {
        if let Some(parent) = path.parent() {
            os.fs.create_dir_all(parent).await?;
        }
        McpServerConfig::default().save_to_file(os, path).await?;
        writeln!(output, "\n📁 Created MCP config in '{}'", path.display())?;
    }

    load_cfg(os, path).await
}

fn parse_env_vars(arg: &str) -> Result<HashMap<String, String>> {
    let mut vars = HashMap::new();

    for pair in arg.split(",") {
        match pair.split_once('=') {
            Some((key, value)) => {
                vars.insert(key.trim().to_string(), value.trim().to_string());
            },
            None => {
                bail!(
                    "Failed to parse environment variables, invalid environment variable '{}'. Expected 'name=value'",
                    pair
                )
            },
        }
    }

    Ok(vars)
}

async fn load_cfg(os: &Os, p: &PathBuf) -> Result<McpServerConfig> {
    Ok(if os.fs.exists(p) {
        McpServerConfig::load_from_file(os, p).await?
    } else {
        McpServerConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RootSubcommand;
    use crate::util::test::assert_parse;

    #[tokio::test]
    async fn test_scope_and_profile_defaults_to_workspace() {
        let os = Os::new().await.unwrap();
        let path = resolve_scope_profile(&os, None, None).unwrap();
        assert_eq!(
            path.to_str(),
            workspace_mcp_config_path(&os).unwrap().to_str(),
            "No scope or profile should default to the workspace path"
        );
    }

    #[tokio::test]
    async fn test_resolve_paths() {
        let os = Os::new().await.unwrap();
        // workspace
        let p = resolve_scope_profile(&os, Some(Scope::Workspace), None).unwrap();
        assert_eq!(p, workspace_mcp_config_path(&os).unwrap());

        // global
        let p = resolve_scope_profile(&os, Some(Scope::Global), None).unwrap();
        assert_eq!(p, global_mcp_config_path(&os).unwrap());
    }

    #[ignore = "TODO: fix in CI"]
    #[tokio::test]
    async fn ensure_file_created_and_loaded() {
        let os = Os::new().await.unwrap();
        let path = workspace_mcp_config_path(&os).unwrap();

        let cfg = super::ensure_config_file(&os, &path, &mut vec![]).await.unwrap();
        assert!(path.exists(), "config file should be created");
        assert!(cfg.mcp_servers.is_empty());
    }

    #[tokio::test]
    async fn add_then_remove_cycle() {
        let os = Os::new().await.unwrap();

        // 1. add
        AddArgs {
            name: "local".into(),
            command: "echo hi".into(),
            args: vec![
                "awslabs.eks-mcp-server".to_string(),
                "--allow-write".to_string(),
                "--allow-sensitive-data-access".to_string(),
            ],
            env: vec![],
            timeout: None,
            scope: None,
            profile: None,
            disabled: false,
            force: false,
        }
        .execute(&os, &mut vec![])
        .await
        .unwrap();

        let cfg_path = workspace_mcp_config_path(&os).unwrap();
        let cfg: McpServerConfig =
            serde_json::from_str(&os.fs.read_to_string(cfg_path.clone()).await.unwrap()).unwrap();
        assert!(cfg.mcp_servers.len() == 1);

        // 2. remove
        RemoveArgs {
            name: "local".into(),
            scope: None,
            profile: None,
        }
        .execute(&os, &mut vec![])
        .await
        .unwrap();

        let cfg: McpServerConfig = serde_json::from_str(&os.fs.read_to_string(cfg_path).await.unwrap()).unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn test_mcp_subcomman_add() {
        assert_parse!(
            [
                "mcp",
                "add",
                "--name",
                "test_server",
                "--command",
                "test_command",
                "--args",
                "awslabs.eks-mcp-server,--allow-write,--allow-sensitive-data-access",
                "--env",
                "key1=value1,key2=value2"
            ],
            RootSubcommand::Mcp(McpSubcommand::Add(AddArgs {
                name: "test_server".to_string(),
                command: "test_command".to_string(),
                args: vec![
                    "awslabs.eks-mcp-server".to_string(),
                    "--allow-write".to_string(),
                    "--allow-sensitive-data-access".to_string(),
                ],
                scope: None,
                profile: None,
                env: vec![
                    [
                        ("key1".to_string(), "value1".to_string()),
                        ("key2".to_string(), "value2".to_string())
                    ]
                    .into_iter()
                    .collect()
                ],
                timeout: None,
                disabled: false,
                force: false,
            }))
        );
    }

    #[test]
    fn test_mcp_subcomman_remove_workspace() {
        assert_parse!(
            ["mcp", "remove", "--name", "old"],
            RootSubcommand::Mcp(McpSubcommand::Remove(RemoveArgs {
                name: "old".into(),
                scope: None,
                profile: None,
            }))
        );
    }

    #[test]
    fn test_mcp_subcomman_import_profile_force() {
        assert_parse!(
            ["mcp", "import", "--file", "servers.json", "--force"],
            RootSubcommand::Mcp(McpSubcommand::Import(ImportArgs {
                file: "servers.json".into(),
                scope: None,
                profile: None,
                force: true,
            }))
        );
    }

    #[test]
    fn test_mcp_subcommand_status_simple() {
        assert_parse!(
            ["mcp", "status", "--name", "aws"],
            RootSubcommand::Mcp(McpSubcommand::Status(StatusArgs { 
                name: "aws".into(),
                profile: None,
            }))
        );
    }

    #[test]
    fn test_mcp_subcommand_list() {
        assert_parse!(
            ["mcp", "list", "global"],
            RootSubcommand::Mcp(McpSubcommand::List(ListArgs {
                scope: Some(Scope::Global),
                profile: None
            }))
        );
    }
}
