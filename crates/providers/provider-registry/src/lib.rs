//! Provider registration independent of Codex and optional network/auth implementations.
use std::collections::BTreeMap;
use std::sync::OnceLock;

use codex_provider_api::ProviderError;
pub use codex_provider_api::RecoveryPolicy;
use codex_provider_api::Result;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::AuthDriver;
#[cfg(feature = "runtime")]
pub use runtime::NativeLogin;
#[cfg(feature = "runtime")]
pub use runtime::TransportFactory;
#[cfg(feature = "runtime")]
pub use runtime::TransportOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    ApiKey,
    OAuth,
    Cursor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub image_generation: bool,
    pub web_search: bool,
    pub external_web_access: bool,
}

/// Metadata stays available even when the provider's transport is not compiled.
#[derive(Debug, Clone, Copy)]
pub struct Descriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub env_key: Option<&'static str>,
    pub auxiliary_model: &'static str,
    pub legacy_wire_aliases: &'static [&'static str],
    pub authentication: Authentication,
    pub capabilities: Capabilities,
    pub recovery_policy: RecoveryPolicy,
    pub native_operations: bool,
    #[cfg(feature = "runtime")]
    pub factory: Option<runtime::TransportFactory>,
    #[cfg(feature = "runtime")]
    pub auth_driver: Option<&'static runtime::AuthDriver>,
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    descriptors: BTreeMap<&'static str, &'static Descriptor>,
}
impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, descriptor: &'static Descriptor) -> Result<()> {
        if descriptor.id.is_empty() || descriptor.id.trim() != descriptor.id {
            return Err(ProviderError::Protocol(
                "provider ID must be nonempty without surrounding whitespace".into(),
            ));
        }
        if self.descriptors.contains_key(descriptor.id)
            || self.iter().any(|existing| {
                existing
                    .legacy_wire_aliases
                    .iter()
                    .any(|alias| descriptor.legacy_wire_aliases.contains(alias))
            })
        {
            return Err(ProviderError::Protocol(format!(
                "duplicate provider registration: {}",
                descriptor.id
            )));
        }
        self.descriptors.insert(descriptor.id, descriptor);
        Ok(())
    }
    pub fn get(&self, id: &str) -> Option<&'static Descriptor> {
        self.descriptors.get(id).copied()
    }
    pub fn iter(&self) -> impl Iterator<Item = &'static Descriptor> + '_ {
        self.descriptors.values().copied()
    }
    pub fn resolve_legacy(&self, alias: &str) -> Option<&'static Descriptor> {
        self.iter()
            .find(|descriptor| descriptor.legacy_wire_aliases.contains(&alias))
    }
}

pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        descriptors: BUILT_INS
            .iter()
            .map(|descriptor| (descriptor.id, *descriptor))
            .collect(),
    })
}

const NO_CAPABILITIES: Capabilities = Capabilities {
    image_generation: false,
    web_search: false,
    external_web_access: false,
};

pub static CURSOR: Descriptor = Descriptor {
    id: "cursor",
    display_name: "Cursor",
    env_key: None,
    auxiliary_model: "gpt-5.6-luna-none",
    legacy_wire_aliases: &["cursor"],
    authentication: Authentication::Cursor,
    capabilities: NO_CAPABILITIES,
    recovery_policy: RecoveryPolicy::RebuildFromHistory,
    native_operations: true,
    #[cfg(all(feature = "runtime", feature = "provider-cursor"))]
    factory: Some(runtime::cursor_factory),
    #[cfg(all(feature = "runtime", not(feature = "provider-cursor")))]
    factory: None,
    #[cfg(all(feature = "runtime", feature = "provider-cursor"))]
    auth_driver: Some(&runtime::CURSOR_AUTH),
    #[cfg(all(feature = "runtime", not(feature = "provider-cursor")))]
    auth_driver: None,
};

pub static OPENCODE: Descriptor = Descriptor {
    id: "opencode-go",
    display_name: "OpenCode Go",
    env_key: Some("OPENCODE_API_KEY"),
    auxiliary_model: "glm-5.3",
    legacy_wire_aliases: &["opencode-go"],
    authentication: Authentication::ApiKey,
    capabilities: NO_CAPABILITIES,
    recovery_policy: RecoveryPolicy::RestorePrevious,
    native_operations: false,
    #[cfg(all(feature = "runtime", feature = "provider-opencode"))]
    factory: Some(runtime::opencode_factory),
    #[cfg(all(feature = "runtime", not(feature = "provider-opencode")))]
    factory: None,
    #[cfg(all(feature = "runtime", feature = "provider-opencode"))]
    auth_driver: Some(&runtime::OPENCODE_AUTH),
    #[cfg(all(feature = "runtime", not(feature = "provider-opencode")))]
    auth_driver: None,
};

pub static CLAUDE: Descriptor = Descriptor {
    id: "claude",
    display_name: "Claude Pro/Max",
    env_key: None,
    auxiliary_model: "claude-haiku-5-5",
    legacy_wire_aliases: &["claude"],
    authentication: Authentication::OAuth,
    capabilities: NO_CAPABILITIES,
    recovery_policy: RecoveryPolicy::RestorePrevious,
    native_operations: false,
    #[cfg(all(feature = "runtime", feature = "provider-claude"))]
    factory: Some(runtime::claude_factory),
    #[cfg(all(feature = "runtime", not(feature = "provider-claude")))]
    factory: None,
    #[cfg(all(feature = "runtime", feature = "provider-claude"))]
    auth_driver: Some(&runtime::CLAUDE_AUTH),
    #[cfg(all(feature = "runtime", not(feature = "provider-claude")))]
    auth_driver: None,
};

pub static ANTIGRAVITY: Descriptor = Descriptor {
    id: "antigravity",
    display_name: "Antigravity",
    env_key: None,
    auxiliary_model: "gemini-3.5-flash-lite",
    legacy_wire_aliases: &["antigravity"],
    authentication: Authentication::OAuth,
    capabilities: NO_CAPABILITIES,
    recovery_policy: RecoveryPolicy::RestorePrevious,
    native_operations: false,
    #[cfg(all(feature = "runtime", feature = "provider-antigravity"))]
    factory: Some(runtime::antigravity_factory),
    #[cfg(all(feature = "runtime", not(feature = "provider-antigravity")))]
    factory: None,
    #[cfg(all(feature = "runtime", feature = "provider-antigravity"))]
    auth_driver: Some(&runtime::ANTIGRAVITY_AUTH),
    #[cfg(all(feature = "runtime", not(feature = "provider-antigravity")))]
    auth_driver: None,
};

pub static BUILT_INS: &[&Descriptor] = &[&CURSOR, &OPENCODE, &CLAUDE, &ANTIGRAVITY];

/// Iterates all registered built-in provider metadata, including disabled transports.
pub fn descriptors() -> impl Iterator<Item = &'static Descriptor> {
    registry().iter()
}

pub fn get(id: &str) -> Option<&'static Descriptor> {
    registry().get(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_registration_does_not_replace_descriptor() {
        let mut registry = Registry::new();
        registry.register(&CURSOR).unwrap();
        assert!(registry.register(&CURSOR).is_err());
        assert!(std::ptr::eq(registry.get("cursor").unwrap(), &CURSOR));
    }

    #[test]
    fn metadata_is_available_for_every_builtin() {
        assert_eq!(registry().iter().count(), BUILT_INS.len());
        for descriptor in BUILT_INS {
            assert!(registry().get(descriptor.id).is_some());
            assert_eq!(
                registry().resolve_legacy(descriptor.id).unwrap().id,
                descriptor.id
            );
        }
        assert!(registry().get("unknown").is_none());
    }
    #[cfg(feature = "runtime")]
    #[test]
    fn registered_disabled_transports_report_not_compiled() {
        for descriptor in descriptors().filter(|descriptor| descriptor.factory.is_none()) {
            let error = match descriptor.provider(TransportOptions {
                base_url: None,
                state_home: None,
            }) {
                Ok(_) => panic!("disabled provider unexpectedly constructed a transport"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("was not compiled"));
            assert!(get(descriptor.id).is_some());
        }
        assert!(get("unknown-native-provider").is_none());
    }
}
