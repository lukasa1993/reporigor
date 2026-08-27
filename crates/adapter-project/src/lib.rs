//! Safe project-provider discovery for TypeScript, `SwiftPM`, Python, and Bash.
//!
//! [`providers`] and [`ProjectAdapter::discover`] inspect only the filesystem
//! and `PATH`; they never spawn a process. Toolchain commands run only through
//! the explicit [`ProjectAdapter::preflight`] API.

mod command;
mod metadata;
mod provider;

pub use command::{CommandRunner, ProviderCommand, ProviderCommandOutput, SystemCommandRunner};
pub use metadata::{discover_bash_dialects, ShellDialect};
pub use provider::{
    providers, providers_with_options, ProjectAdapter, ProviderOptions, ProviderProvenance,
    ProviderResolution, ProviderStatus,
};
