use crate::feature_flags::FeatureFlags;
use crate::git::repository::Repository;
use crate::mdm::utils::home_dir;
use glob::Pattern;
use serde::{Deserialize, Serialize, Serializer};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(any(test, feature = "test-support"))]
use std::sync::RwLock;

pub const DEFAULT_API_BASE_URL: &str = "https://usegitai.com";
pub(crate) const MEBIBYTE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_DAEMON_MEMORY_LIMIT_MB: u64 = u64::MAX / MEBIBYTE_BYTES;

/// Which Codex hook file git-ai should use when installing Codex hooks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexHooksFormat {
    /// Default: install git-ai Codex hooks inline in ~/.codex/config.toml.
    #[default]
    ConfigToml,
    /// Install git-ai Codex hooks in ~/.codex/hooks.json.
    HooksJson,
}

impl CodexHooksFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodexHooksFormat::ConfigToml => "config_toml",
            CodexHooksFormat::HooksJson => "hooks_json",
        }
    }

    fn from_str(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "config_toml" | "config-toml" => Some(CodexHooksFormat::ConfigToml),
            "hooks_json" | "hooks-json" => Some(CodexHooksFormat::HooksJson),
            _ => None,
        }
    }
}

impl std::fmt::Display for CodexHooksFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Serialize)]
pub struct Config {
    git_path: String,
    #[serde(serialize_with = "serialize_patterns")]
    allow_repositories: Vec<Pattern>,
    #[serde(serialize_with = "serialize_patterns")]
    exclude_repositories: Vec<Pattern>,
    telemetry_oss_disabled: bool,
    telemetry_enterprise_dsn: Option<String>,
    disable_version_checks: bool,
    disable_auto_updates: bool,
    update_channel: UpdateChannel,
    feature_flags: FeatureFlags,
    api_base_url: String,
    #[serde(serialize_with = "serialize_masked_api_key")]
    api_key: Option<String>,
    quiet: bool,
    allow_superuser: bool,
    codex_hooks_format: CodexHooksFormat,
    daemon_memory_limit_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    #[default]
    Latest,
    Next,
    EnterpriseLatest,
    EnterpriseNext,
}

impl UpdateChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdateChannel::Latest => "latest",
            UpdateChannel::Next => "next",
            UpdateChannel::EnterpriseLatest => "enterprise-latest",
            UpdateChannel::EnterpriseNext => "enterprise-next",
        }
    }

    fn from_str(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "latest" => Some(UpdateChannel::Latest),
            "next" => Some(UpdateChannel::Next),
            "enterprise-latest" => Some(UpdateChannel::EnterpriseLatest),
            "enterprise-next" => Some(UpdateChannel::EnterpriseNext),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
pub struct FileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_oss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_enterprise_dsn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_version_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_auto_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_superuser: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_hooks_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_memory_limit_mb: Option<u64>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static TEST_FEATURE_FLAGS_OVERRIDE: RwLock<Option<FeatureFlags>> = RwLock::new(None);

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_version_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_auto_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_hooks_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_memory_limit_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_oss_disabled: Option<bool>,
}

impl Config {
    /// Initialize the global configuration exactly once.
    /// Safe to call multiple times; subsequent calls are no-ops.
    #[allow(dead_code)]
    pub fn init() {
        let _ = CONFIG.get_or_init(build_config);
    }

    /// Access the global configuration. Lazily initializes if not already initialized.
    pub fn get() -> &'static Config {
        CONFIG.get_or_init(build_config)
    }

    /// Build a fresh config snapshot from disk/env without using the global cache.
    ///
    /// This is useful for long-lived daemon processes that must observe runtime
    /// config updates (for example, API credentials and delivery settings).
    pub fn fresh() -> Self {
        build_config()
    }

    pub fn git_cmd(&self) -> &str {
        &self.git_path
    }

    pub fn has_repository_filters(&self) -> bool {
        !self.allow_repositories.is_empty() || !self.exclude_repositories.is_empty()
    }

    pub fn is_allowed_repository(&self, repository: &Option<Repository>) -> bool {
        // Fetch remotes once and reuse for both exclude and allow checks
        let remotes = repository
            .as_ref()
            .and_then(|repo| repo.remotes_with_urls().ok());

        self.is_allowed_repository_with_remotes(remotes.as_ref())
    }

    /// Helper that accepts pre-fetched remotes to avoid multiple git operations
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn is_allowed_repository_with_remotes(
        &self,
        remotes: Option<&Vec<(String, String)>>,
    ) -> bool {
        if remotes.is_some_and(|remotes| {
            remotes.iter().any(|(_, remote_url)| {
                crate::diagnostic_sentinels::is_debug_self_check_remote_url(remote_url)
            })
        }) {
            return true;
        }

        // First check if repository is in exclusion list - exclusions take precedence
        if !self.exclude_repositories.is_empty()
            && let Some(remotes) = remotes
        {
            // If any remote matches the exclusion patterns, deny access
            if remotes
                .iter()
                .any(|remote| remote_matches_patterns(&self.exclude_repositories, &remote.1))
            {
                return false;
            }
        }

        // If allowlist is empty, allow everything (unless excluded above)
        if self.allow_repositories.is_empty() {
            return true;
        }

        // If allowlist is defined, only allow repos whose remotes match the patterns
        match remotes {
            Some(remotes) => remotes
                .iter()
                .any(|remote| remote_matches_patterns(&self.allow_repositories, &remote.1)),
            None => false, // Can't verify, deny by default when allowlist is active
        }
    }

    pub fn is_telemetry_oss_disabled(&self) -> bool {
        self.telemetry_oss_disabled
    }

    /// Returns the telemetry_enterprise_dsn if set.
    pub fn telemetry_enterprise_dsn(&self) -> Option<&str> {
        self.telemetry_enterprise_dsn.as_deref()
    }

    pub fn version_checks_disabled(&self) -> bool {
        self.disable_version_checks
    }

    pub fn auto_updates_disabled(&self) -> bool {
        self.disable_auto_updates
    }

    pub fn update_channel(&self) -> UpdateChannel {
        self.update_channel
    }

    pub fn feature_flags(&self) -> &FeatureFlags {
        &self.feature_flags
    }

    /// Returns the API base URL
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub fn daemon_memory_limit_mb(&self) -> Option<u64> {
        self.daemon_memory_limit_mb
    }

    /// Returns true if quiet mode is enabled.
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub fn allow_superuser(&self) -> bool {
        self.allow_superuser
    }

    pub fn codex_hooks_format(&self) -> CodexHooksFormat {
        self.codex_hooks_format
    }

    /// Serialize the effective runtime config into pretty JSON.
    /// Sensitive values are redacted via field serializers.
    pub fn to_printable_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize runtime config: {}", e))
    }

    /// Override feature flags for testing purposes.
    /// Only available when the `test-support` feature is enabled or in test mode.
    /// Must be `pub` to work with integration tests in the `tests/` directory.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub fn set_test_feature_flags(flags: FeatureFlags) {
        let mut override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .write()
            .expect("Failed to acquire write lock on test feature flags");
        *override_flags = Some(flags);
    }

    /// Clear any feature flag overrides.
    /// Only available when the `test-support` feature is enabled or in test mode.
    /// This should be called in test cleanup to reset to default behavior.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub fn clear_test_feature_flags() {
        let mut override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .write()
            .expect("Failed to acquire write lock on test feature flags");
        *override_flags = None;
    }

    /// Get feature flags, checking for test overrides first.
    /// In test mode, this will return overridden flags if set, otherwise the normal flags.
    #[cfg(any(test, feature = "test-support"))]
    pub fn get_feature_flags(&self) -> FeatureFlags {
        let override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .read()
            .expect("Failed to acquire read lock on test feature flags");
        override_flags
            .clone()
            .unwrap_or_else(|| self.feature_flags.clone())
    }

    /// Get feature flags (non-test version, just returns a reference).
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn get_feature_flags(&self) -> &FeatureFlags {
        &self.feature_flags
    }
}

fn serialize_patterns<S>(patterns: &[Pattern], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let as_strings: Vec<&str> = patterns.iter().map(Pattern::as_str).collect();
    as_strings.serialize(serializer)
}

fn remote_matches_patterns(patterns: &[Pattern], remote_url: &str) -> bool {
    let remote_candidates = repo_remote_match_candidates(remote_url);
    patterns.iter().any(|pattern| {
        repo_pattern_match_candidates(pattern.as_str())
            .iter()
            .filter_map(|candidate| Pattern::new(candidate).ok())
            .any(|candidate_pattern| {
                remote_candidates
                    .iter()
                    .any(|candidate| candidate_pattern.matches(candidate))
            })
    })
}

fn repo_pattern_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn repo_remote_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
            candidates.push(path);
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn repo_match_parts(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();

    if let Some((_, rest)) = value.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        return Some((
            normalize_repo_authority(authority)?,
            normalize_repo_path_variants(path)?,
        ));
    }

    let (user_host, path) = value.split_once(':')?;
    if value.starts_with('/') || !user_host.contains('@') || path.is_empty() {
        return None;
    }

    let (_, host) = user_host.rsplit_once('@')?;
    Some((
        normalize_repo_host(host)?,
        normalize_repo_path_variants(path)?,
    ))
}

fn normalize_repo_authority(authority: &str) -> Option<String> {
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    normalize_repo_host(host)
}

fn normalize_repo_host(host: &str) -> Option<String> {
    let host = strip_repo_host_port(host.trim());
    if host.is_empty() {
        return None;
    }

    let host = host.to_ascii_lowercase();
    if matches!(host.as_str(), "dev.azure.com" | "ssh.dev.azure.com") {
        Some("azure".to_string())
    } else {
        Some(host)
    }
}

fn strip_repo_host_port(host: &str) -> &str {
    if let Some(stripped) = strip_bracketed_host_port(host) {
        return stripped;
    }

    let Some((host_without_port, port)) = host.rsplit_once(':') else {
        return host;
    };
    if host_without_port.contains(':') || port.is_empty() {
        host
    } else {
        host_without_port
    }
}

fn strip_bracketed_host_port(host: &str) -> Option<&str> {
    let rest = host.strip_prefix('[')?;
    let bracket_index = rest.find(']')?;
    let bracket_end = bracket_index + 2;
    let after_bracket = host.get(bracket_end..)?;

    if after_bracket.is_empty() || after_bracket.starts_with(':') {
        Some(&host[..bracket_end])
    } else {
        None
    }
}

fn normalize_repo_path_variants(path: &str) -> Option<Vec<String>> {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_start_matches('/')
        .trim_end_matches('/')
        .trim_end_matches(".git");

    if path.is_empty() {
        return None;
    }

    let mut variants = vec![path.to_string()];
    let segments: Vec<&str> = path.split('/').collect();
    if segments.first() == Some(&"v3") && segments.len() > 1 {
        variants.push(segments[1..].join("/"));
    }
    if let Some(git_segment_index) = segments.iter().position(|segment| *segment == "_git")
        && git_segment_index > 0
        && git_segment_index + 1 < segments.len()
    {
        let mut without_git_segment = segments.clone();
        without_git_segment.remove(git_segment_index);
        variants.push(without_git_segment.join("/"));
    }

    variants.sort();
    variants.dedup();
    Some(variants)
}

fn serialize_masked_api_key<S>(api_key: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let masked = api_key.as_ref().map(|key| {
        let chars: Vec<char> = key.chars().collect();
        if chars.len() > 8 {
            let prefix: String = chars[..4].iter().collect();
            let suffix: String = chars[chars.len() - 4..].iter().collect();
            format!("{}...{}", prefix, suffix)
        } else {
            "****".to_string()
        }
    });
    masked.serialize(serializer)
}

fn build_config() -> Config {
    let file_cfg = load_file_config();
    let allow_repositories = parse_path_patterns(
        "allow_repositories",
        file_cfg
            .as_ref()
            .and_then(|c| c.allow_repositories.clone())
            .unwrap_or_default(),
    );
    let exclude_repositories = parse_path_patterns(
        "exclude_repositories",
        file_cfg
            .as_ref()
            .and_then(|c| c.exclude_repositories.clone())
            .unwrap_or_default(),
    );
    let telemetry_oss_disabled = file_cfg
        .as_ref()
        .and_then(|c| c.telemetry_oss.clone())
        .filter(|s| s == "off")
        .is_some();
    let telemetry_enterprise_dsn = file_cfg
        .as_ref()
        .and_then(|c| c.telemetry_enterprise_dsn.clone())
        .filter(|s| !s.is_empty());

    // Default to disabled (true) unless this is an OSS build
    // OSS builds set OSS_BUILD env var at compile time to "1", which enables auto-updates by default
    let auto_update_flags_default_disabled = option_env!("OSS_BUILD") != Some("1");

    let disable_version_checks = file_cfg
        .as_ref()
        .and_then(|c| c.disable_version_checks)
        .unwrap_or(auto_update_flags_default_disabled);
    let disable_auto_updates = file_cfg
        .as_ref()
        .and_then(|c| c.disable_auto_updates)
        .unwrap_or(auto_update_flags_default_disabled);
    let update_channel = file_cfg
        .as_ref()
        .and_then(|c| c.update_channel.as_deref())
        .and_then(UpdateChannel::from_str)
        .unwrap_or_default();

    let git_path = resolve_git_path(&file_cfg);

    // Build feature flags from file config
    let feature_flags = build_feature_flags(&file_cfg);

    let api_base_url = env::var("GIT_AI_API_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| file_cfg.as_ref().and_then(|c| c.api_base_url.clone()))
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.into());
    // Get API key from env var or config file (env var takes precedence)
    let api_key = env::var("GIT_AI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_cfg
                .as_ref()
                .and_then(|c| c.api_key.clone())
                .filter(|s| !s.is_empty())
        });

    // Get quiet setting (defaults to false)
    let quiet = file_cfg.as_ref().and_then(|c| c.quiet).unwrap_or(false);

    let allow_superuser = file_cfg
        .as_ref()
        .and_then(|c| c.allow_superuser)
        .unwrap_or(false);

    let codex_hooks_format = file_cfg
        .as_ref()
        .and_then(|c| c.codex_hooks_format.as_deref())
        .and_then(|value| {
            let parsed = CodexHooksFormat::from_str(value);
            if parsed.is_none() {
                eprintln!(
                    "Warning: Invalid codex_hooks_format value '{}', using 'config_toml'",
                    value
                );
            }
            parsed
        })
        .unwrap_or_default();

    let daemon_memory_limit_mb = file_cfg
        .as_ref()
        .and_then(|c| c.daemon_memory_limit_mb)
        .and_then(normalize_daemon_memory_limit_mb);

    let config = Config {
        git_path,
        allow_repositories,
        exclude_repositories,
        telemetry_oss_disabled,
        telemetry_enterprise_dsn,
        disable_version_checks,
        disable_auto_updates,
        update_channel,
        feature_flags,
        api_base_url,
        api_key,
        quiet,
        allow_superuser,
        codex_hooks_format,
        daemon_memory_limit_mb,
    };
    #[cfg(any(test, feature = "test-support"))]
    {
        let mut config = config;
        apply_test_config_patch(&mut config);
        config
    }
    #[cfg(not(any(test, feature = "test-support")))]
    config
}

fn normalize_daemon_memory_limit_mb(limit_mb: u64) -> Option<u64> {
    if limit_mb == 0 {
        eprintln!(
            "Warning: daemon_memory_limit_mb must be greater than zero; memory monitoring is disabled"
        );
        return None;
    }
    if limit_mb > MAX_DAEMON_MEMORY_LIMIT_MB {
        eprintln!("Warning: daemon_memory_limit_mb is too large; memory monitoring is disabled");
        return None;
    }
    Some(limit_mb)
}

fn build_feature_flags(file_cfg: &Option<FileConfig>) -> FeatureFlags {
    let flags = file_cfg
        .as_ref()
        .and_then(|c| c.feature_flags.clone())
        .and_then(|v| serde_json::from_value(v).ok());
    FeatureFlags::from_env_and_file(flags)
}

fn resolve_git_path(file_cfg: &Option<FileConfig>) -> String {
    // 1) From config file
    if let Some(cfg) = file_cfg
        && let Some(path) = cfg.git_path.as_ref()
    {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            let p = Path::new(trimmed);
            if is_executable(p) && !path_is_git_ai_binary(p) {
                return trimmed.to_string();
            }
        }
    }

    // 2) Probe common locations across platforms.
    // All candidates are guarded by path_is_git_ai_binary so that a git-ai shim at any
    // of these locations can never be returned as the "real git" (fork bomb prevention).
    #[cfg(not(windows))]
    let local_bin_git = format!("{}/.local/bin/git", home_dir().display());

    #[cfg(windows)]
    let local_app_data_candidates: Vec<String> = std::env::var("LOCALAPPDATA")
        .ok()
        .map(|lad| {
            vec![
                format!(r"{}\Programs\Git\cmd\git.exe", lad),
                format!(r"{}\Programs\Git\bin\git.exe", lad),
            ]
        })
        .unwrap_or_default();

    let static_candidates: &[&str] = &[
        #[cfg(not(windows))]
        local_bin_git.as_str(),
        #[cfg(not(windows))]
        "/opt/homebrew/bin/git",
        #[cfg(not(windows))]
        "/usr/local/bin/git",
        #[cfg(not(windows))]
        "/usr/bin/git",
        #[cfg(not(windows))]
        "/bin/git",
        #[cfg(not(windows))]
        "/usr/local/sbin/git",
        #[cfg(not(windows))]
        "/usr/sbin/git",
        #[cfg(windows)]
        r"C:\Program Files\Git\cmd\git.exe",
        #[cfg(windows)]
        r"C:\Program Files\Git\bin\git.exe",
        #[cfg(windows)]
        r"C:\Program Files (x86)\Git\cmd\git.exe",
        #[cfg(windows)]
        r"C:\Program Files (x86)\Git\bin\git.exe",
    ];

    #[cfg(windows)]
    let all_candidates: Vec<&str> = {
        let mut v: Vec<&str> = static_candidates.to_vec();
        for c in &local_app_data_candidates {
            v.push(c.as_str());
        }
        v
    };

    #[cfg(windows)]
    let candidates: &[&str] = &all_candidates;
    #[cfg(not(windows))]
    let candidates: &[&str] = static_candidates;

    if let Some(found) = candidates
        .iter()
        .map(Path::new)
        .find(|p| is_executable(p) && !path_is_git_ai_binary(p))
    {
        return found.to_string_lossy().to_string();
    }

    // 3) Windows-only: try `where.exe git.exe` as a PATH-based fallback
    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("where.exe")
            .arg("git.exe")
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                let p = Path::new(trimmed);
                if is_executable(p) && !path_is_git_ai_binary(p) {
                    return trimmed.to_string();
                }
            }
        }
    }

    eprintln!(
        "Fatal: Could not locate a real 'git' binary.\n\
         Expected a valid 'git_path' in {cfg_path} or in standard locations.\n\
         Please install Git or update your config JSON.",
        cfg_path = config_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "~/.git-ai/config.json".to_string()),
    );
    std::process::exit(1);
}

fn load_file_config() -> Option<FileConfig> {
    let path = config_file_path()?;
    let data = fs::read(&path).ok()?;
    parse_file_config_bytes(&data).ok()
}

fn parse_file_config_bytes(data: &[u8]) -> Result<FileConfig, serde_json::Error> {
    // Windows PowerShell 5.1 writes UTF-8 with BOM by default for `Out-File -Encoding UTF8`.
    // Tolerate BOM-prefixed config files so upgrades/installers don't brick config parsing.
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    serde_json::from_slice::<FileConfig>(data)
}

fn config_file_path() -> Option<PathBuf> {
    Some(home_dir().join(".git-ai").join("config.json"))
}

/// Public accessor for config file path
#[allow(dead_code)]
pub fn config_file_path_public() -> Option<PathBuf> {
    config_file_path()
}

/// Returns the path to the git-ai base directory (~/.git-ai)
pub fn git_ai_dir_path() -> Option<PathBuf> {
    Some(home_dir().join(".git-ai"))
}

/// Returns the path to the internal state directory (~/.git-ai/internal)
/// This is where git-ai stores internal files like distinct_id, update_check, etc.
pub fn internal_dir_path() -> Option<PathBuf> {
    git_ai_dir_path().map(|dir| dir.join("internal"))
}

/// Public accessor for ID file path (~/.git-ai/internal/distinct_id)
pub fn id_file_path() -> Option<PathBuf> {
    internal_dir_path().map(|dir| dir.join("distinct_id"))
}

/// Cache for the distinct_id to avoid repeated file reads
static DISTINCT_ID: OnceLock<String> = OnceLock::new();

/// Get or create the distinct_id (UUID) from ~/.git-ai/internal/distinct_id
/// If the file doesn't exist, generates a new UUID and writes it to the file.
/// The result is cached for the lifetime of the process.
pub fn get_or_create_distinct_id() -> String {
    DISTINCT_ID
        .get_or_init(|| {
            let id_path = match id_file_path() {
                Some(path) => path,
                None => return "unknown".to_string(),
            };

            // Try to read existing ID
            if let Ok(existing_id) = fs::read_to_string(&id_path) {
                let trimmed = existing_id.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }

            // Generate new UUID
            let new_id = crate::uuid::generate_v4();

            // Ensure directory exists
            if let Some(parent) = id_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            // Write the new ID to file
            if let Err(e) = fs::write(&id_path, &new_id) {
                eprintln!("Warning: Failed to write distinct_id file: {}", e);
            }

            new_id
        })
        .clone()
}

/// Returns the path to the update check cache file (~/.git-ai/internal/update_check)
pub fn update_check_path() -> Option<PathBuf> {
    internal_dir_path().map(|dir| dir.join("update_check"))
}

/// Load the raw file config
pub fn load_file_config_public() -> Result<FileConfig, String> {
    let path =
        config_file_path().ok_or_else(|| "Could not determine config file path".to_string())?;

    if !path.exists() {
        // Return empty config if file doesn't exist
        return Ok(FileConfig::default());
    }

    let data = fs::read(&path).map_err(|e| format!("Failed to read config file: {}", e))?;

    parse_file_config_bytes(&data).map_err(|e| format!("Failed to parse config file: {}", e))
}

/// Save the file config
pub fn save_file_config(config: &FileConfig) -> Result<(), String> {
    let path =
        config_file_path().ok_or_else(|| "Could not determine config file path".to_string())?;

    // Ensure the directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    fs::write(&path, json).map_err(|e| format!("Failed to write config file: {}", e))
}

fn is_executable(path: &Path) -> bool {
    if !path.exists() || !path.is_file() {
        return false;
    }
    // Basic check: existence is sufficient for our purposes; OS will enforce exec perms.
    // On Unix we could check permissions, but many filesystems differ. Keep it simple.
    true
}

/// Check whether two paths refer to the same underlying file.
/// On Unix this compares (dev, ino); on other platforms it falls back to
/// comparing canonicalized paths.
#[cfg(not(windows))]
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    #[cfg(not(unix))]
    {
        if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
            return ca == cb;
        }
    }
    false
}

/// Detect if a path is actually the git-ai binary (or a symlink to it).
/// This prevents `git_cmd()` from returning the git-ai shim, which would
/// cause infinite recursion: handle_git() → proxy_to_git() → shim → handle_git() → ...
fn path_is_git_ai_binary(path: &Path) -> bool {
    // Check canonical path — if the path resolves to a binary whose name
    // is git-ai (or a variant), it is the git-ai binary regardless of what
    // the original path looks like (catches symlinks like `git → git-ai`).
    if let Ok(canonical) = path.canonicalize()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
    {
        let stem = name.strip_suffix(".exe").unwrap_or(name);
        if stem == "git-ai" || stem.starts_with("git-ai-") || stem.starts_with("git_ai") {
            return true;
        }
    }

    // Check if a sibling "git-ai" exists in the same directory.
    // On Windows the installer copies git-ai.exe to git.exe (not a symlink or
    // hard-link), so same_file() would return false. A sibling git-ai.exe
    // existing is sufficient to identify this as the git-ai install directory.
    // On Unix, additionally verify both refer to the same underlying file
    // (hard-link / bind-mount) to avoid false-positives in environments where
    // a real git binary legitimately coexists with a git-ai symlink (e.g.
    // Docker images that compile git from source into /usr/local/bin).
    if let Some(parent) = path.parent() {
        #[cfg(windows)]
        let sibling = parent.join("git-ai.exe");
        #[cfg(not(windows))]
        let sibling = parent.join("git-ai");

        #[cfg(windows)]
        if sibling.exists() {
            return true;
        }
        #[cfg(not(windows))]
        if sibling.exists() && same_file(path, &sibling) {
            return true;
        }
    }

    false
}

/// Returns true if `p` is an executable git binary that is NOT git-ai.
/// Used by test infrastructure to probe for the real git binary independently
/// of `Config::get()` (which reads HOME and must not be called before HOME is isolated).
pub fn is_real_git_candidate(p: &Path) -> bool {
    is_executable(p) && !path_is_git_ai_binary(p)
}

/// Glob patterns from a config list, warning about and dropping invalid ones.
fn parse_path_patterns(key: &str, patterns: Vec<String>) -> Vec<Pattern> {
    patterns
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in {key} '{}': {}",
                        pattern_str, e
                    );
                })
                .ok()
        })
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
fn apply_test_config_patch(config: &mut Config) {
    if let Ok(value) = env::var("GIT_AI_TEST_CONFIG_PATCH")
        && let Ok(patch) = serde_json::from_str::<ConfigPatch>(&value)
    {
        if let Some(value) = patch.git_path {
            config.git_path = value;
        }
        if let Some(value) = patch.telemetry_oss_disabled {
            config.telemetry_oss_disabled = value;
        }
        if let Some(value) = patch.disable_auto_updates {
            config.disable_auto_updates = value;
        }
        if let Some(value) = patch.disable_version_checks {
            config.disable_version_checks = value;
        }
        if let Some(value) = patch.allow_repositories {
            config.allow_repositories = parse_path_patterns("allow_repositories", value);
        }
        if let Some(value) = patch.exclude_repositories {
            config.exclude_repositories = parse_path_patterns("exclude_repositories", value);
        }
        if let Some(value) = patch.daemon_memory_limit_mb {
            config.daemon_memory_limit_mb = normalize_daemon_memory_limit_mb(value);
        }
        if let Some(value) = patch
            .codex_hooks_format
            .and_then(|v| CodexHooksFormat::from_str(&v))
        {
            config.codex_hooks_format = value;
        }
        if let Some(value) = patch
            .feature_flags
            .and_then(|v| serde_json::from_value(v).ok())
        {
            config.feature_flags = FeatureFlags::merge_with(config.feature_flags.clone(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn create_test_config(
        allow_repositories: Vec<String>,
        exclude_repositories: Vec<String>,
    ) -> Config {
        Config {
            git_path: "/usr/bin/git".to_string(),
            allow_repositories: allow_repositories
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            exclude_repositories: exclude_repositories
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            telemetry_oss_disabled: false,
            telemetry_enterprise_dsn: None,
            disable_version_checks: false,
            disable_auto_updates: false,
            update_channel: UpdateChannel::Latest,
            feature_flags: FeatureFlags::default(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            api_key: None,
            quiet: false,
            allow_superuser: false,
            codex_hooks_format: CodexHooksFormat::ConfigToml,
            daemon_memory_limit_mb: None,
        }
    }

    #[test]
    fn test_exclusion_takes_precedence_over_allow() {
        let config = create_test_config(
            vec!["https://github.com/allowed/repo".to_string()],
            vec!["https://github.com/allowed/repo".to_string()],
        );

        // Test with None repository - should return false when allowlist is active
        assert!(!config.is_allowed_repository(&None));
    }

    #[test]
    fn test_empty_allowlist_allows_everything() {
        let config = create_test_config(vec![], vec![]);

        // With empty allowlist, should allow everything
        assert!(config.is_allowed_repository(&None));
    }

    #[test]
    fn test_exclude_without_allow() {
        let config =
            create_test_config(vec![], vec!["https://github.com/excluded/repo".to_string()]);

        // With empty allowlist but exclusions, should allow everything (exclusions only matter when checking remotes)
        assert!(config.is_allowed_repository(&None));
    }

    #[test]
    fn test_allow_without_exclude() {
        let config =
            create_test_config(vec!["https://github.com/allowed/repo".to_string()], vec![]);

        // With allowlist but no exclusions, should deny when no repository provided
        assert!(!config.is_allowed_repository(&None));
    }

    #[test]
    fn test_glob_pattern_wildcard_in_allow() {
        let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);

        // Test that the pattern would match (note: we can't easily test with real Repository objects,
        // but the pattern compilation is tested by the fact that create_test_config succeeds)
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("https://github.com/myorg/repo1"));
        assert!(config.allow_repositories[0].matches("https://github.com/myorg/repo2"));
        assert!(!config.allow_repositories[0].matches("https://github.com/other/repo"));
    }

    #[test]
    fn test_glob_pattern_wildcard_in_exclude() {
        let config = create_test_config(vec![], vec!["https://github.com/private/*".to_string()]);

        // Test pattern matching
        assert!(!config.exclude_repositories.is_empty());
        assert!(config.exclude_repositories[0].matches("https://github.com/private/repo1"));
        assert!(config.exclude_repositories[0].matches("https://github.com/private/secret"));
        assert!(!config.exclude_repositories[0].matches("https://github.com/public/repo"));
    }

    #[test]
    fn test_exact_match_still_works() {
        let config = create_test_config(vec!["https://github.com/exact/match".to_string()], vec![]);

        // Test that exact matches still work (glob treats them as literals)
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("https://github.com/exact/match"));
        assert!(!config.allow_repositories[0].matches("https://github.com/exact/other"));
    }

    #[test]
    fn test_complex_glob_patterns() {
        let config = create_test_config(vec!["*@github.com:company/*".to_string()], vec![]);

        // Test more complex patterns with wildcards
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("git@github.com:company/repo"));
        assert!(config.allow_repositories[0].matches("user@github.com:company/project"));
        assert!(!config.allow_repositories[0].matches("git@github.com:other/repo"));
    }

    #[test]
    fn test_remote_pattern_matching_normalizes_common_git_url_forms() {
        let scp_patterns = vec![Pattern::new("git@github.com:company/*").unwrap()];
        assert!(remote_matches_patterns(
            &scp_patterns,
            "ssh://git@github.com/company/repo"
        ));
        assert!(remote_matches_patterns(
            &scp_patterns,
            "ssh://git@github.com:22/company/repo"
        ));
        assert!(!remote_matches_patterns(
            &scp_patterns,
            "ssh://git@github.com/other/repo"
        ));
        assert!(remote_matches_patterns(
            &scp_patterns,
            "https://github.com/company/repo"
        ));
        assert!(remote_matches_patterns(
            &scp_patterns,
            "git://github.com/company/repo.git"
        ));

        let ssh_patterns = vec![Pattern::new("ssh://git@github.com/company/*").unwrap()];
        assert!(remote_matches_patterns(
            &ssh_patterns,
            "git@github.com:company/repo"
        ));
        assert!(remote_matches_patterns(
            &ssh_patterns,
            "https://github.com/company/repo.git"
        ));

        let ssh_port_patterns = vec![Pattern::new("ssh://git@github.com:2222/company/*").unwrap()];
        assert!(remote_matches_patterns(
            &ssh_port_patterns,
            "git@github.com:company/repo"
        ));
        assert!(remote_matches_patterns(
            &ssh_port_patterns,
            "ssh://git@github.com:2022/company/repo"
        ));

        let https_patterns = vec![Pattern::new("https://github.com/company/*").unwrap()];
        assert!(remote_matches_patterns(
            &https_patterns,
            "ssh://git@github.com:2022/company/repo"
        ));
        assert!(remote_matches_patterns(
            &https_patterns,
            "git@github.com:company/repo.git"
        ));
    }

    #[test]
    fn test_remote_pattern_matching_allows_hostless_repository_patterns() {
        let patterns = vec![Pattern::new("company/*").unwrap()];

        assert!(remote_matches_patterns(
            &patterns,
            "https://github.com/company/repo"
        ));
        assert!(remote_matches_patterns(
            &patterns,
            "git@gitlab.com:company/repo.git"
        ));
        assert!(!remote_matches_patterns(
            &patterns,
            "https://github.com/other/repo"
        ));
    }

    #[test]
    fn test_remote_pattern_matching_handles_azure_https_and_ssh_shape_difference() {
        let https_patterns =
            vec![Pattern::new("https://dev.azure.com/acme/widgets/_git/*").unwrap()];
        assert!(remote_matches_patterns(
            &https_patterns,
            "ssh://git@ssh.dev.azure.com:22/v3/acme/widgets/service"
        ));

        let ssh_patterns =
            vec![Pattern::new("ssh://git@ssh.dev.azure.com/v3/acme/widgets/*").unwrap()];
        assert!(remote_matches_patterns(
            &ssh_patterns,
            "https://dev.azure.com/acme/widgets/_git/service"
        ));
    }
}
