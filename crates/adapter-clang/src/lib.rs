//! Clang compilation-database project adapter.
//!
//! The adapter deliberately never invokes a shell or a build generator. It
//! reads an existing `compile_commands.json`, resolves the translation units
//! that belong to the requested project, and asks a configured Clang binary to
//! validate each unit with `-fsyntax-only` and a bounded timeout.

mod ast;
mod backend;
mod command;
mod database;
mod validation;

pub use ast::{AstDumpStatus, AstTranslationUnit, ClangAnalysis};
pub use backend::{ClangAdapter, ClangProject};
pub use command::{
    sanitize_compile_command, strip_output_and_dependency_flags, tokenize_command, CommandTokenizeError,
    SanitizedCommand, SanitizedCommandError,
};
pub use database::{
    discover_compilation_database, load_database, ClangAdapterError, ClangLanguage, CommandOrigin,
    CompilationDatabase, CompileCommand,
};
pub use validation::{TranslationUnit, ValidationStatus};
