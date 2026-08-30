//! Safe project-provider discovery for TypeScript, `SwiftPM`, Python, and Bash.
//!
//! [`providers`] and [`ProjectAdapter::discover`] inspect only the filesystem
//! and `PATH`; they never spawn a process. Toolchain commands run only through
//! the explicit [`ProjectAdapter::preflight`] API.

mod command;
mod metadata;
mod provider;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

pub use command::{CommandRunner, ProviderCommand, ProviderCommandOutput, SystemCommandRunner};
pub use metadata::{discover_bash_dialects, ShellDialect};
pub use mutation_discovery::discover_mutation_providers;
pub use mutation_preflight::preflight_mutation_providers;
pub use provider::{
    providers, providers_with_options, ProjectAdapter, ProviderOptions, ProviderProvenance,
    ProviderResolution, ProviderStatus,
};
pub use provider_mutation::{ImportFormat, MutationProviderStatus, ProviderInventory};

/// Discover optional mutation-report providers without executing commands.
mod mutation_discovery {
    use super::ProviderInventory;

    /// # Errors
    ///
    /// Returns an error when the project root cannot be inspected safely.
    pub fn discover_mutation_providers(
        root: &std::path::Path,
    ) -> Result<ProviderInventory, provider_mutation::ProviderError> {
        provider_mutation::discover(root)
    }
}

/// Discover and run only the mutation providers' read-only version probes.
mod mutation_preflight {
    use super::ProviderInventory;

    /// # Errors
    ///
    /// Returns an error when discovery or a required safe probe cannot complete.
    pub fn preflight_mutation_providers(
        root: &std::path::Path,
    ) -> Result<ProviderInventory, provider_mutation::ProviderError> {
        let inventory = provider_mutation::preflight(root)?;
        Ok(inventory)
    }
}
