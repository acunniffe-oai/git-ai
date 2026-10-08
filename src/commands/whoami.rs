use crate::api::{ApiClient, ApiContext};
use crate::auth::state::AuthStatus;
use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::config;
use std::fmt::Write as _;

pub fn handle_whoami(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        print_help();
        std::process::exit(0);
    }

    if !args.is_empty() {
        eprintln!("Error: unknown whoami argument(s): {}", args.join(" "));
        print_help();
        std::process::exit(1);
    }

    // Use Config::fresh() to support runtime config updates (daemon mode).
    let config = config::Config::fresh();
    let api_base_url = config.api_base_url().to_string();
    let telemetry_oss_disabled = config.is_telemetry_oss_disabled();
    let auth = collect_auth_status();
    let api_ctx = ApiContext::new(None);
    let api_client = ApiClient::new(api_ctx.clone());

    print!(
        "{}",
        render_whoami(
            &api_base_url,
            &auth,
            &api_ctx,
            &api_client,
            telemetry_oss_disabled,
        )
    );

    if should_exit_failure(&auth, &api_ctx) {
        std::process::exit(1);
    }
}

fn render_whoami(
    api_base_url: &str,
    auth: &AuthStatus,
    api_ctx: &ApiContext,
    api_client: &ApiClient,
    telemetry_oss_disabled: bool,
) -> String {
    let mut out = String::new();

    writeln!(out, "git-ai status").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "API").unwrap();
    writeln!(out, "  Base URL: {}", api_base_url).unwrap();
    writeln!(
        out,
        "  API access: {}",
        api_access_status(auth, api_ctx, api_client)
    )
    .unwrap();
    writeln!(
        out,
        "  Identity: {}",
        author_identity_header_status(api_ctx)
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "Authentication").unwrap();
    writeln!(
        out,
        "  API key: {}",
        api_key_status(api_ctx.api_key.as_deref())
    )
    .unwrap();
    writeln!(out, "  Login: {}", login_status(auth, api_client)).unwrap();
    writeln!(out, "  Credential backend: {}", auth.backend).unwrap();
    if let Some(expires_at) = auth.access_token_expires_at {
        writeln!(
            out,
            "  Access token expires at: {}",
            format_unix_timestamp(expires_at)
        )
        .unwrap();
    }
    if let Some(expires_at) = auth.refresh_token_expires_at {
        writeln!(
            out,
            "  Refresh token expires at: {}",
            format_unix_timestamp(expires_at)
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    if matches!(auth.state, AuthState::LoggedIn) {
        writeln!(out, "Login identity").unwrap();
        writeln!(out, "  User ID: {}", optional_or_unavailable(&auth.user_id)).unwrap();
        writeln!(out, "  Email: {}", optional_or_unavailable(&auth.email)).unwrap();
        writeln!(out, "  Name: {}", optional_or_unavailable(&auth.name)).unwrap();
        writeln!(
            out,
            "  Personal org ID: {}",
            optional_or_unavailable(&auth.personal_org_id)
        )
        .unwrap();
        if auth.orgs.is_empty() {
            writeln!(out, "  Organizations: <none>").unwrap();
        } else {
            writeln!(out, "  Organizations:").unwrap();
            for org in &auth.orgs {
                let org_id = org.org_id.as_deref().unwrap_or("<unknown-id>");
                let org_slug = org.org_slug.as_deref().unwrap_or("<unknown-slug>");
                let org_name = org.org_name.as_deref().unwrap_or("<unknown-name>");
                let role = org.role.as_deref().unwrap_or("<unknown-role>");
                writeln!(
                    out,
                    "    - {} ({}) [{}] role={}",
                    org_slug, org_name, org_id, role
                )
                .unwrap();
            }
        }
        writeln!(out).unwrap();
    }

    writeln!(out, "Telemetry status").unwrap();
    writeln!(
        out,
        "  Commit-link delivery: {}",
        if api_client.has_api_key() {
            "on (API key configured)"
        } else {
            "off (requires an API key)"
        }
    )
    .unwrap();
    writeln!(
        out,
        "  OSS diagnostic telemetry: {}",
        if telemetry_oss_disabled { "off" } else { "on" }
    )
    .unwrap();

    out
}

fn api_access_status(auth: &AuthStatus, api_ctx: &ApiContext, api_client: &ApiClient) -> String {
    match (api_client.has_api_key(), api_client.is_logged_in()) {
        (true, true) => "connected via API key and login".to_string(),
        (true, false) => "connected via API key".to_string(),
        (false, true) => "connected via login".to_string(),
        (false, false) => {
            if matches!(auth.state, AuthState::LoggedIn) {
                return "not connected (login credentials found, but no usable access token)"
                    .to_string();
            }
            if api_ctx.base_url != crate::config::DEFAULT_API_BASE_URL {
                "no auth credential found (custom API URL configured)".to_string()
            } else {
                "not connected".to_string()
            }
        }
    }
}

fn should_exit_failure(auth: &AuthStatus, api_ctx: &ApiContext) -> bool {
    api_ctx.api_key.is_none() && !matches!(auth.state, AuthState::LoggedIn)
}

fn author_identity_header_status(api_ctx: &ApiContext) -> String {
    if api_ctx.api_key.is_none() {
        return "not sent (API key not configured)".to_string();
    }

    api_ctx
        .author_identity
        .clone()
        .unwrap_or_else(|| "unavailable (header will be omitted)".to_string())
}

fn api_key_status(api_key: Option<&str>) -> String {
    api_key
        .map(|key| format!("configured ({})", mask_api_key(key)))
        .unwrap_or_else(|| "unset".to_string())
}

fn login_status(auth: &AuthStatus, api_client: &ApiClient) -> String {
    match &auth.state {
        AuthState::LoggedIn => "logged in".to_string(),
        AuthState::LoggedOut | AuthState::RefreshExpired | AuthState::Error(_) => {
            if api_client.has_api_key() {
                "optional (using api key)".to_string()
            } else {
                "not logged in".to_string()
            }
        }
    }
}

fn optional_or_unavailable(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("<unavailable>")
}

fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}...{}", prefix, suffix)
}

fn print_help() {
    eprintln!("git-ai whoami - Show current auth, identity, and telemetry status");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  git-ai whoami");
    eprintln!("  git-ai whoami --help");
}
