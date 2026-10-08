use serde::{Deserialize, Serialize};

/// Parse a boolean from an environment variable string.
/// Accepts: "1", "true", "True", "TRUE" → Some(true)
///          "0", "false", "False", "FALSE" → Some(false)
///          Missing or invalid → None
fn parse_bool_env(value: Option<String>) -> Option<bool> {
    value.and_then(|v| match v.as_str() {
        "1" | "true" | "True" | "TRUE" => Some(true),
        "0" | "false" | "False" | "FALSE" => Some(false),
        _ => None,
    })
}

macro_rules! define_feature_flags {
    (
        $(
            $field:ident: $file_name:ident, debug = $debug_default:expr, release = $release_default:expr
        ),* $(,)?
    ) => {
        /// Feature flags for the application
        #[derive(Debug, Clone, Serialize)]
        pub struct FeatureFlags {
            $(pub $field: bool,)*
        }

        impl Default for FeatureFlags {
            fn default() -> Self {
                #[cfg(debug_assertions)]
                {
                    return FeatureFlags {
                        $($field: $debug_default,)*
                    };
                }
                #[cfg(not(debug_assertions))]
                FeatureFlags {
                    $($field: $release_default,)*
                }
            }
        }

        /// Deserializable version of FeatureFlags with all optional fields
        /// Works for both file config and environment variables
        #[derive(Deserialize, Default)]
        #[serde(default)]
        pub(crate) struct DeserializableFeatureFlags {
            $(
                #[serde(default)]
                $file_name: Option<bool>,
            )*
        }

        impl DeserializableFeatureFlags {
            /// Parse feature flags from environment variables with the given prefix.
            /// Converts field names to SCREAMING_SNAKE_CASE.
            pub(crate) fn from_env(prefix: &str) -> Self {
                DeserializableFeatureFlags {
                    $(
                        $file_name: parse_bool_env(
                            std::env::var(format!("{}{}", prefix, stringify!($file_name).to_uppercase())).ok()
                        ),
                    )*
                }
            }
        }

        impl FeatureFlags {
            /// Merge flags with a base, applying any Some values as overrides
            pub(crate) fn merge_with(base: Self, overrides: DeserializableFeatureFlags) -> Self {
                FeatureFlags {
                    $($field: overrides.$file_name.unwrap_or(base.$field),)*
                }
            }
        }
    };
}

// Define all feature flags in one place
// Format: struct_field: file_and_env_name, debug = <bool>, release = <bool>
define_feature_flags!(
    auth_keyring: auth_keyring, debug = false, release = false,
    daemon_log_upload: daemon_log_upload, debug = true, release = true,
);

impl FeatureFlags {
    /// Build FeatureFlags from deserializable config
    fn from_deserializable(flags: DeserializableFeatureFlags) -> Self {
        Self::merge_with(FeatureFlags::default(), flags)
    }

    /// Build FeatureFlags from environment variables
    /// Reads from GIT_AI_* prefixed environment variables
    /// Example: GIT_AI_AUTH_KEYRING=true
    /// Falls back to defaults for any invalid or missing values
    #[allow(dead_code)]
    pub fn from_env() -> Self {
        let env_flags = DeserializableFeatureFlags::from_env("GIT_AI_");
        Self::from_deserializable(env_flags)
    }

    /// Build FeatureFlags from both file and environment variables
    /// Precedence: Environment > File > Default
    /// - Starts with defaults
    /// - Applies file config overrides if present
    /// - Applies environment variable overrides if present (highest priority)
    pub(crate) fn from_env_and_file(file_flags: Option<DeserializableFeatureFlags>) -> Self {
        // Start with defaults
        let mut result = FeatureFlags::default();

        // Apply file config overrides
        if let Some(file) = file_flags {
            result = Self::merge_with(result, file);
        }

        // Apply env var overrides (highest priority)
        let env_flags = DeserializableFeatureFlags::from_env("GIT_AI_");
        result = Self::merge_with(result, env_flags);

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_defaults_and_file_overrides() {
        let defaults = FeatureFlags::default();
        assert!(!defaults.auth_keyring);
        assert!(defaults.daemon_log_upload);
        let flags = FeatureFlags::from_deserializable(DeserializableFeatureFlags {
            auth_keyring: Some(true),
            daemon_log_upload: Some(false),
        });
        assert!(flags.auth_keyring);
        assert!(!flags.daemon_log_upload);
    }
    #[test]
    fn ignored_legacy_flag_values_do_not_break_retained_flags() {
        let file = serde_json::from_str::<DeserializableFeatureFlags>(
            r#"{"transcript_streaming":"ignored","auth_keyring":true}"#,
        )
        .unwrap();
        assert!(FeatureFlags::from_deserializable(file).auth_keyring);
    }
}
