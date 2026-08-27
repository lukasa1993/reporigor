use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::{ClangLanguage, CompileCommand};

/// A tokenization failure for a compilation-database `command` string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandTokenizeError {
    #[error("command contains a NUL byte")]
    Nul,
    #[error("unterminated {0} quote")]
    UnterminatedQuote(&'static str),
    #[error("command ends with an incomplete escape")]
    IncompleteEscape,
    #[error("shell operator or expansion {token:?} is not supported")]
    ShellSyntax { token: char },
    #[error("command is empty")]
    Empty,
}

/// A failure while turning a compile command into a non-writing Clang syntax
/// validation command.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SanitizedCommandError {
    #[error("compile command has no executable")]
    MissingExecutable,
    #[error("compile command contains unsafe compiler argument {0:?}")]
    UnsafeArgument(String),
    #[error("failed to create an isolated Clang scratch directory: {0}")]
    ScratchDirectory(String),
}

/// The exact process invocation used for translation-unit validation.
#[derive(Debug, Clone)]
pub struct SanitizedCommand {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub directory: PathBuf,
    scratch: Option<Arc<tempfile::TempDir>>,
}

impl PartialEq for SanitizedCommand {
    fn eq(&self, other: &Self) -> bool {
        self.program == other.program
            && self.arguments == other.arguments
            && self.directory == other.directory
            && self.scratch_directory() == other.scratch_directory()
    }
}

impl Eq for SanitizedCommand {}

impl SanitizedCommand {
    pub(crate) fn direct(program: PathBuf, arguments: Vec<String>, directory: PathBuf) -> Self {
        Self {
            program,
            arguments,
            directory,
            scratch: None,
        }
    }

    pub(crate) fn scratch_directory(&self) -> Option<&Path> {
        self.scratch.as_deref().map(tempfile::TempDir::path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Split a POSIX-style compilation command without invoking a shell.
///
/// Quoting and escaping used for paths and macro definitions are supported.
/// Shell control operators and expansions are rejected rather than interpreted
/// with subtly different semantics. Compilation databases that provide an
/// `arguments` array bypass tokenization entirely, as recommended by the
/// compilation-database specification.
///
/// # Errors
///
/// Returns an error for empty commands, malformed quoting/escaping, NUL bytes,
/// or shell operators and expansions that cannot be represented safely as
/// direct process arguments.
pub fn tokenize_command(command: &str) -> Result<Vec<String>, CommandTokenizeError> {
    if command.as_bytes().contains(&0) {
        return Err(CommandTokenizeError::Nul);
    }

    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut token_started = false;

    for character in command.chars() {
        if escaped {
            token.push(character);
            token_started = true;
            escaped = false;
            continue;
        }

        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    token.push(character);
                }
                token_started = true;
            }
            Quote::Double => match character {
                '"' => {
                    quote = Quote::None;
                    token_started = true;
                }
                '\\' => {
                    escaped = true;
                    token_started = true;
                }
                '$' | '`' | '\n' | '\r' => {
                    return Err(CommandTokenizeError::ShellSyntax { token: character });
                }
                _ => {
                    token.push(character);
                    token_started = true;
                }
            },
            Quote::None => match character {
                '\'' => {
                    quote = Quote::Single;
                    token_started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    token_started = true;
                }
                '\\' => {
                    escaped = true;
                    token_started = true;
                }
                ' ' | '\t' => {
                    if token_started {
                        tokens.push(std::mem::take(&mut token));
                        token_started = false;
                    }
                }
                '\n' | '\r' | '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' => {
                    return Err(CommandTokenizeError::ShellSyntax { token: character });
                }
                _ => {
                    token.push(character);
                    token_started = true;
                }
            },
        }
    }

    if escaped {
        return Err(CommandTokenizeError::IncompleteEscape);
    }
    match quote {
        Quote::Single => return Err(CommandTokenizeError::UnterminatedQuote("single")),
        Quote::Double => return Err(CommandTokenizeError::UnterminatedQuote("double")),
        Quote::None => {}
    }
    if token_started {
        tokens.push(token);
    }
    if tokens.is_empty() {
        return Err(CommandTokenizeError::Empty);
    }
    Ok(tokens)
}

/// Remove flags that select a compile/output mode or write dependency and
/// diagnostics artifacts. The source path, include paths, definitions, target,
/// language, and other semantic flags remain unchanged.
#[must_use]
pub fn strip_output_and_dependency_flags(arguments: &[String]) -> Vec<String> {
    let mut result = Vec::with_capacity(arguments.len());
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];

        if flag_takes_separate_value(argument) {
            index = index.saturating_add(2);
            continue;
        }
        if is_joined_output_flag(argument)
            || is_dependency_mode_flag(argument)
            || is_output_mode_flag(argument)
        {
            index += 1;
            continue;
        }

        // Clang's driver forwards some output flags through `-Xclang`. Remove
        // the complete forwarded pair while preserving unrelated frontend
        // arguments.
        if argument == "-Xclang" && index + 1 < arguments.len() {
            let forwarded = &arguments[index + 1];
            if flag_takes_separate_value(forwarded) {
                index += 2;
                if index < arguments.len() && arguments[index] == "-Xclang" {
                    index += 1;
                    if index < arguments.len() {
                        index += 1;
                    }
                } else if index < arguments.len() {
                    index += 1;
                }
                continue;
            }
            if is_joined_output_flag(forwarded)
                || is_dependency_mode_flag(forwarded)
                || is_output_mode_flag(forwarded)
            {
                index += 2;
                continue;
            }
        }

        result.push(argument.clone());
        index += 1;
    }
    result
}

fn flag_takes_separate_value(argument: &str) -> bool {
    matches!(
        argument,
        "-o" | "--output"
            | "/o"
            | "-as-secure-log-file"
            | "-analyzer-dump-egraph"
            | "-gen-cdb-fragment-path"
            | "-dependency-dot"
            | "-dsym-dir"
            | "-dumpdir"
            | "-MF"
            | "-MT"
            | "-MQ"
            | "-MJ"
            | "-dependency-file"
            | "--dependency-file"
            | "-diagnostic-log-file"
            | "-fapinotes-cache-path"
            | "-fcrash-diagnostics-dir"
            | "-fmodules-cache-path"
            | "-fmodules-user-build-path"
            | "-header-include-file"
            | "-index-store-path"
            | "-index-unit-output-path"
            | "-module-dependency-dir"
            | "-opt-record-file"
            | "-serialize-diagnostics"
            | "--serialize-diagnostics"
            | "-serialize-diagnostic-file"
            | "-split-dwarf-file"
            | "-split-dwarf-output"
            | "-stack-usage-file"
            | "-stats-file"
            | "--symbol-graph-dir"
            | "-foptimization-record-file"
    )
}

fn is_joined_output_flag(argument: &str) -> bool {
    let joined = [
        "--output=",
        "-as-secure-log-file=",
        "-analyzer-dump-egraph=",
        "-gen-cdb-fragment-path=",
        "-dependency-dot=",
        "-dsym-dir=",
        "-dumpdir=",
        "-MF",
        "-MT",
        "-MQ",
        "-MJ",
        "-dependency-file=",
        "--dependency-file=",
        "-diagnostic-log-file=",
        "-fapinotes-cache-path=",
        "-fcrash-diagnostics-dir=",
        "-fmodules-cache-path=",
        "-fmodules-user-build-path=",
        "-header-include-file=",
        "-index-store-path=",
        "-index-unit-output-path=",
        "-module-dependency-dir=",
        "-opt-record-file=",
        "-serialize-diagnostics=",
        "--serialize-diagnostics=",
        "-serialize-diagnostic-file=",
        "-split-dwarf-file=",
        "-split-dwarf-output=",
        "-stack-usage-file=",
        "-stats-file=",
        "--symbol-graph-dir=",
        "-foptimization-record-file=",
        "-fmodule-output=",
    ];
    joined
        .iter()
        .any(|prefix| argument.starts_with(prefix) && argument.len() > prefix.len())
        || (argument.starts_with("-o")
            && argument.len() > 2
            && !["-objc", "-object", "-offload", "-omit", "-openmp", "-opt"]
                .iter()
                .any(|semantic| argument.starts_with(semantic)))
}

fn is_dependency_mode_flag(argument: &str) -> bool {
    matches!(argument, "-M" | "-MM" | "-MD" | "-MMD" | "-MG" | "-MP")
}

fn is_output_mode_flag(argument: &str) -> bool {
    matches!(
        argument,
        "-c" | "-S"
            | "-E"
            | "--analyze"
            | "-analyze"
            | "--precompile"
            | "-extract-api"
            | "-rewrite-legacy-objc"
            | "-rewrite-objc"
            | "-via-file-asm"
            | "-gen-reproducer"
            | "-fmodule-output"
            | "-fcrash-diagnostics"
            | "-fno-crash-diagnostics"
            | "-fno-temp-file"
            | "-fprofile-arcs"
            | "-fsave-optimization-record"
            | "-fstack-usage"
            | "-ftest-coverage"
            | "-save-temps"
            | "--save-temps"
            | "-save-stats"
            | "-ftime-trace"
            | "-index-record-codegen-name"
            | "-index-store-compress"
            | "-stats-file-append"
            | "--emit-extension-symbol-graphs"
            | "--gpu-bundle-output"
            | "-fdiagnostics-parseable-fixits"
            | "-fdiagnostics-print-source-range-info"
    ) || argument.starts_with("-emit-")
        || argument.starts_with("--emit-")
        || argument.starts_with("-dump-depscan-tree=")
        || argument.starts_with("-gen-reproducer=")
        || argument.starts_with("-fcodegen-data-generate")
        || argument.starts_with("-fcrash-diagnostics=")
        || argument.starts_with("-fcs-profile-generate")
        || argument == "-fmemory-profile"
        || argument.starts_with("-fmemory-profile=")
        || argument.starts_with("-fproc-stat-report")
        || argument.starts_with("-fprofile-generate")
        || argument.starts_with("-fprofile-instr-generate")
        || argument.starts_with("-fsave-optimization-record=")
        || argument.starts_with("-save-stats=")
        || argument.starts_with("-save-temps=")
        || argument.starts_with("--save-temps=")
        || argument.starts_with("-ftime-trace=")
        || is_clang_cl_output_flag(argument)
}

fn is_clang_cl_output_flag(argument: &str) -> bool {
    matches!(
        argument,
        "/c" | "/E" | "/EP" | "/LD" | "/LDd" | "/P" | "/Zs" | "/FA"
    ) || [
        "/doc",
        "/Fa",
        "/Fd",
        "/Fe",
        "/Fi",
        "/Fm",
        "/Fo",
        "/FR",
        "/Fr",
        "/ifcOutput",
        "/module:output",
        "/sourceDependencies",
        "/Yc",
    ]
    .iter()
    .any(|prefix| argument.starts_with(prefix))
}

fn executable_basename(value: &str) -> &str {
    Path::new(value)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(value)
}

fn is_compiler_driver(value: &str) -> bool {
    let basename = executable_basename(value).to_ascii_lowercase();
    let without_extension = basename.strip_suffix(".exe").unwrap_or(&basename);
    let stem = without_extension
        .split_once('-')
        .map_or(without_extension, |(prefix, suffix)| {
            if prefix.chars().all(|character| character.is_ascii_digit()) {
                suffix
            } else {
                without_extension
            }
        });
    let drivers = [
        "clang", "clang++", "gcc", "g++", "cc", "c++", "icc", "icx", "icpx",
    ];
    drivers.iter().any(|driver| {
        stem == *driver || stem.starts_with(&format!("{driver}-")) || stem.ends_with(&format!("-{driver}"))
    })
}

fn compiler_argument_start(arguments: &[String]) -> Result<usize, SanitizedCommandError> {
    if arguments.is_empty() {
        return Err(SanitizedCommandError::MissingExecutable);
    }
    if let Some(index) = arguments.iter().position(|argument| is_compiler_driver(argument)) {
        return Ok(index + 1);
    }
    // Custom compiler drivers are valid compilation-database executables. The
    // first token is still the executable by specification; it is never run.
    Ok(1)
}

fn is_flag_or_joined_value(argument: &str, flag: &str) -> bool {
    argument == flag
        || argument
            .strip_prefix(flag)
            .is_some_and(|suffix| suffix.starts_with('='))
}

fn is_unsafe_compiler_argument(argument: &str) -> bool {
    // Response files are expanded by the driver before normal option parsing.
    if argument.starts_with('@') {
        return true;
    }

    // clang-cl passes every remaining argument after `/link` to a linker. It
    // is an unbounded response/plugin/output escape rather than syntax input.
    if argument.eq_ignore_ascii_case("/link") {
        return true;
    }

    // These forwarding mechanisms can smuggle a response file or a `-load`
    // option to cc1/LLVM. Deny the mechanism, rather than maintaining a
    // necessarily incomplete allow-list of their evolving payload languages.
    if matches!(
        argument,
        "-Xanalyzer" | "-Xclang" | "-Xclangas" | "-Xpreprocessor" | "-mllvm"
    ) || [
        "-Xanalyzer=",
        "-Xclang=",
        "-Xclangas=",
        "-Xpreprocessor=",
        "-mllvm=",
        "-Wp,",
    ]
    .iter()
    .any(|prefix| argument.starts_with(prefix))
        || argument.starts_with("-cc1")
        || argument
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/clang:"))
    {
        return true;
    }

    // Driver configuration files can contain every other compiler option,
    // including plugin loaders. Directory overrides are rejected with the
    // explicit file selector so a database cannot redirect default lookup.
    if [
        "--config",
        "--config-system-dir",
        "--config-user-dir",
        "-multi-lib-config",
    ]
    .iter()
    .any(|flag| is_flag_or_joined_value(argument, flag))
    {
        return true;
    }

    // Clang's dependency scanner and CAS modes can populate persistent stores
    // or start a sharing daemon. They are project-build facilities, not syntax
    // semantics, and have no safe database-controlled destination here.
    if argument.starts_with("-fdepscan")
        || argument.starts_with("-fcas")
        || argument.starts_with("-fcache-")
        || argument.starts_with("-fexperimental-cache")
    {
        return true;
    }

    // Cover both driver spellings and cc1/LLVM spellings. Exact-or-`=`
    // matching deliberately avoids rejecting benign definitions or paths that
    // merely contain words such as `plugin` or `load`.
    if [
        "-load",
        "--load",
        "-load-pass-plugin",
        "-plugin",
        "-add-plugin",
        "-fplugin",
        "-fpass-plugin",
        "--hipspv-pass-plugin",
        "-fcas-plugin-path",
        "-fcas-plugin-option",
        "-wrapper",
    ]
    .iter()
    .any(|flag| is_flag_or_joined_value(argument, flag))
    {
        return true;
    }

    argument.starts_with("-plugin-arg-") || argument.starts_with("-fplugin-arg-")
}

fn reject_unsafe_compiler_arguments(arguments: &[String]) -> Result<(), SanitizedCommandError> {
    if let Some(argument) = arguments
        .iter()
        .find(|argument| is_unsafe_compiler_argument(argument))
    {
        return Err(SanitizedCommandError::UnsafeArgument(argument.clone()));
    }
    Ok(())
}

fn isolated_scratch() -> Result<(Arc<tempfile::TempDir>, PathBuf), SanitizedCommandError> {
    let scratch = tempfile::Builder::new()
        .prefix("reporigor-clang-")
        .tempdir()
        .map_err(|error| SanitizedCommandError::ScratchDirectory(error.to_string()))?;
    let module_cache = scratch.path().join("module-cache");
    fs::create_dir(&module_cache)
        .map_err(|error| SanitizedCommandError::ScratchDirectory(error.to_string()))?;
    Ok((Arc::new(scratch), module_cache))
}

/// Replace the database's compiler/wrapper with the configured Clang binary,
/// preserve semantic flags, remove writing modes, and add `-fsyntax-only`.
///
/// # Errors
///
/// Returns an error when the original command is empty or requests compiler
/// response/config files, plugins, raw frontend/LLVM forwarding, or wrappers
/// that must not be executed implicitly.
pub fn sanitize_compile_command(
    command: &CompileCommand,
    compiler: &Path,
    language: ClangLanguage,
) -> Result<SanitizedCommand, SanitizedCommandError> {
    let start = compiler_argument_start(&command.arguments)?;
    let original_arguments = &command.arguments[start..];
    reject_unsafe_compiler_arguments(original_arguments)?;
    let mut arguments = strip_output_and_dependency_flags(original_arguments);
    reject_unsafe_compiler_arguments(&arguments)?;
    let (scratch, module_cache) = isolated_scratch()?;

    // Force the classified language so switching from gcc/clang++ or validating
    // an extensionless/header translation unit cannot change semantics. Clang
    // can still write an implicit module cache or a crash reproducer during a
    // syntax-only run, so keep the cache in our owned scratch directory and
    // disable crash artifacts independently of database flags.
    remove_language_flags(&mut arguments);
    arguments.insert(0, language.clang_name().to_string());
    arguments.insert(0, "-x".to_string());
    arguments.insert(2, "-fsyntax-only".to_string());
    arguments.insert(3, "--no-default-config".to_string());
    arguments.insert(4, "-fno-crash-diagnostics".to_string());
    arguments.insert(
        5,
        format!("-fmodules-cache-path={}", module_cache.to_string_lossy()),
    );

    Ok(SanitizedCommand {
        program: compiler.to_path_buf(),
        arguments,
        directory: command.directory.clone(),
        scratch: Some(scratch),
    })
}

fn remove_language_flags(arguments: &mut Vec<String>) {
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "-x" {
            arguments.remove(index);
            if index < arguments.len() {
                arguments.remove(index);
            }
        } else if arguments[index].starts_with("-x") && arguments[index].len() > 2 {
            arguments.remove(index);
        } else {
            index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandOrigin;

    fn command(arguments: &[&str]) -> CompileCommand {
        CompileCommand {
            directory: PathBuf::from("/project"),
            file: PathBuf::from("src/main.c"),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            output: None,
            origin: CommandOrigin::Arguments(arguments.iter().map(ToString::to_string).collect()),
        }
    }

    fn semantic_arguments<'a>(command: &'a SanitizedCommand, language: &str) -> &'a [String] {
        assert_eq!(
            &command.arguments[..5],
            [
                "-x",
                language,
                "-fsyntax-only",
                "--no-default-config",
                "-fno-crash-diagnostics"
            ]
        );
        let scratch = command
            .scratch_directory()
            .unwrap_or_else(|| panic!("sanitized Clang invocation has no owned scratch directory"));
        assert!(scratch.exists());
        assert_eq!(
            command.arguments[5],
            format!(
                "-fmodules-cache-path={}",
                scratch.join("module-cache").to_string_lossy()
            )
        );
        &command.arguments[6..]
    }

    #[test]
    fn tokenizes_quotes_escapes_and_empty_arguments_without_a_shell() {
        let tokens = tokenize_command(r#"clang -I"include dir" '-DNAME=hello world' "" src/main.c"#)
            .unwrap_or_else(|error| panic!("tokenize: {error}"));
        assert_eq!(
            tokens,
            ["clang", "-Iinclude dir", "-DNAME=hello world", "", "src/main.c"]
        );
    }

    #[test]
    fn rejects_shell_control_and_expansion() {
        assert!(matches!(
            tokenize_command("clang a.c && touch marker"),
            Err(CommandTokenizeError::ShellSyntax { token: '&' })
        ));
        assert!(matches!(
            tokenize_command("clang $(malicious)"),
            Err(CommandTokenizeError::ShellSyntax { token: '$' })
        ));
        assert!(matches!(
            tokenize_command("clang 'unterminated"),
            Err(CommandTokenizeError::UnterminatedQuote("single"))
        ));
    }

    #[test]
    fn strips_output_and_dependency_side_effects() {
        let arguments = [
            "-Iinclude",
            "-MD",
            "-MF",
            "main.d",
            "-MTmain.o",
            "-o",
            "main.o",
            "-c",
            "src/main.c",
            "-DVALUE=1",
        ]
        .map(ToString::to_string);
        assert_eq!(
            strip_output_and_dependency_flags(&arguments),
            ["-Iinclude", "src/main.c", "-DVALUE=1"]
        );
    }

    #[test]
    fn sanitizes_wrapped_command_and_forces_language() {
        let command = command(&[
            "ccache",
            "/usr/bin/clang++",
            "-std=c++20",
            "-xobjective-c",
            "-c",
            "src/main.mm",
            "-o",
            "main.o",
        ]);
        let sanitized =
            sanitize_compile_command(&command, Path::new("/usr/bin/clang"), ClangLanguage::ObjectiveCpp)
                .unwrap_or_else(|error| panic!("sanitize: {error}"));
        assert_eq!(sanitized.program, Path::new("/usr/bin/clang"));
        assert_eq!(
            semantic_arguments(&sanitized, "objective-c++"),
            ["-std=c++20", "src/main.mm"]
        );
    }

    fn assert_unsafe(arguments: &[&str]) {
        let command = command(arguments);
        assert!(matches!(
            sanitize_compile_command(&command, Path::new("clang"), ClangLanguage::C),
            Err(SanitizedCommandError::UnsafeArgument(_))
        ));
    }

    #[test]
    fn refuses_response_and_config_file_routes() {
        for arguments in [
            vec!["clang", "@flags.rsp", "main.c"],
            vec!["clang", "--config", "/tmp/evil.cfg", "main.c"],
            vec!["clang", "--config=/tmp/evil.cfg", "main.c"],
            vec!["clang", "--config-system-dir", "/tmp", "main.c"],
            vec!["clang", "--config-system-dir=/tmp", "main.c"],
            vec!["clang", "--config-user-dir", "/tmp", "main.c"],
            vec!["clang", "--config-user-dir=/tmp", "main.c"],
            vec!["clang", "-multi-lib-config", "/tmp/evil.yaml", "main.c"],
            vec!["clang", "-multi-lib-config=/tmp/evil.yaml", "main.c"],
            vec!["clang-cl", "/clang:@flags.rsp", "main.c"],
            vec!["clang-cl", "/clang:--config=/tmp/evil.cfg", "main.c"],
        ] {
            assert_unsafe(&arguments);
        }
    }

    #[test]
    fn refuses_driver_and_cc1_plugin_routes() {
        for arguments in [
            vec!["clang", "-fplugin", "/tmp/plugin.so", "main.c"],
            vec!["clang", "-fplugin=/tmp/plugin.so", "main.c"],
            vec!["clang", "-fpass-plugin", "/tmp/pass.so", "main.c"],
            vec!["clang", "-fpass-plugin=/tmp/pass.so", "main.c"],
            vec!["clang", "--hipspv-pass-plugin=/tmp/pass.so", "main.c"],
            vec!["clang", "-load", "/tmp/plugin.so", "main.c"],
            vec!["clang", "-load=/tmp/plugin.so", "main.c"],
            vec!["clang", "-load-pass-plugin=/tmp/pass.so", "main.c"],
            vec!["clang", "-plugin", "evil", "main.c"],
            vec!["clang", "-plugin=evil", "main.c"],
            vec!["clang", "-add-plugin", "evil", "main.c"],
            vec!["clang", "-add-plugin=evil", "main.c"],
            vec!["clang", "-plugin-arg-evil", "payload", "main.c"],
            vec!["clang", "-fplugin-arg-evil-payload", "main.c"],
            vec!["clang", "-fcas-plugin-path", "/tmp/cas.so", "main.c"],
            vec!["clang", "-wrapper", "/tmp/wrapper", "main.c"],
        ] {
            assert_unsafe(&arguments);
        }
    }

    #[test]
    fn refuses_joined_and_separate_frontend_escape_routes() {
        for arguments in [
            vec!["clang", "-Xclang", "-load", "-Xclang", "/tmp/plugin.so", "main.c"],
            vec!["clang", "-Xclang=-load", "-Xclang=/tmp/plugin.so", "main.c"],
            vec!["clang", "-Xclang=-plugin", "-Xclang=evil", "main.c"],
            vec!["clang", "-Xclang=-add-plugin", "-Xclang=evil", "main.c"],
            vec![
                "clang",
                "-Xclang",
                "-fcas-plugin-path",
                "-Xclang",
                "/tmp/cas.so",
                "main.c",
            ],
            vec![
                "clang",
                "-Xclang=-fcas-plugin-path",
                "-Xclang=/tmp/cas.so",
                "main.c",
            ],
            vec![
                "clang",
                "-Xpreprocessor",
                "-load",
                "-Xpreprocessor",
                "/tmp/plugin.so",
                "main.c",
            ],
            vec!["clang", "-Wp,-load,/tmp/plugin.so", "main.c"],
            vec!["clang", "-Wp,@/tmp/flags.rsp", "main.c"],
            vec![
                "clang",
                "--analyze",
                "-Xanalyzer",
                "-analyzer-dump-egraph",
                "main.c",
            ],
            vec!["clang", "-mllvm", "-load=/tmp/pass.so", "main.c"],
            vec!["clang", "-Xclangas=-load", "main.c"],
            vec!["clang", "-cc1", "-load", "/tmp/plugin.so", "main.c"],
            vec!["clang-cl", "/clang:-fplugin=/tmp/plugin.so", "main.c"],
        ] {
            assert_unsafe(&arguments);
        }
    }

    #[test]
    fn preserves_benign_driver_semantics_without_substring_false_positives() {
        let command = command(&[
            "clang",
            "-std=c17",
            "--target=aarch64-unknown-linux-gnu",
            "--sysroot=/sdk",
            "-Iinclude",
            "-isystem",
            "vendor/include",
            "-include",
            "config.h",
            "-DPLUGIN_NAME=-load",
            "-Wno-pass-failed",
            "-fmodules",
            "src/plugin-config.c",
            "-MD",
            "-MF",
            "main.d",
            "-c",
            "-o",
            "main.o",
        ]);
        let sanitized = sanitize_compile_command(&command, Path::new("clang"), ClangLanguage::C)
            .unwrap_or_else(|error| panic!("sanitize: {error}"));

        assert_eq!(
            semantic_arguments(&sanitized, "c"),
            [
                "-std=c17",
                "--target=aarch64-unknown-linux-gnu",
                "--sysroot=/sdk",
                "-Iinclude",
                "-isystem",
                "vendor/include",
                "-include",
                "config.h",
                "-DPLUGIN_NAME=-load",
                "-Wno-pass-failed",
                "-fmodules",
                "src/plugin-config.c",
            ]
        );
    }

    #[test]
    fn strips_adversarial_write_destinations_and_uses_owned_scratch() {
        let command = command(&[
            "clang",
            "-Iinclude",
            "-fmodules",
            "-fmodules-cache-path=/project/attacker-cache",
            "-fmodule-output=/project/attacker.pcm",
            "-fcrash-diagnostics=all",
            "-fcrash-diagnostics-dir=/project/crash",
            "-gen-reproducer=always",
            "-gen-cdb-fragment-path",
            "/project/cdb",
            "-serialize-diagnostics",
            "/project/diagnostics.dia",
            "-index-store-path",
            "/project/index",
            "-foptimization-record-file=/project/remarks.yaml",
            "-fsave-optimization-record",
            "-ftime-trace=/project/trace.json",
            "-fproc-stat-report=/project/stats.json",
            "-fprofile-instr-generate=/project/default.profraw",
            "-fstack-usage",
            "-emit-symbol-graph",
            "--symbol-graph-dir=/project/symbols",
            "-MD",
            "-MF",
            "/project/main.d",
            "-c",
            "-o",
            "/project/main.o",
            "src/main.c",
        ]);

        let sanitized = sanitize_compile_command(&command, Path::new("clang"), ClangLanguage::C)
            .unwrap_or_else(|error| panic!("sanitize: {error}"));

        assert_eq!(
            semantic_arguments(&sanitized, "c"),
            ["-Iinclude", "-fmodules", "src/main.c"]
        );
        assert!(sanitized
            .arguments
            .iter()
            .all(|argument| !argument.contains("/project/")));
    }

    #[test]
    fn refuses_persistent_dependency_scanner_and_cas_modes() {
        for unsafe_argument in [
            "-fdepscan",
            "-fdepscan=daemon",
            "-fdepscan-daemon=/project/socket",
            "-fcas-path=/project/cache",
            "-fcas-backend",
            "-fcache-compile-job",
        ] {
            assert_unsafe(&["clang", unsafe_argument, "main.c"]);
        }
    }

    #[test]
    fn rejects_each_raw_unsafe_argument() {
        for unsafe_argument in ["@flags.rsp", "-fplugin=/tmp/plugin.so", "-load"] {
            let command = command(&["clang", unsafe_argument, "main.c"]);
            assert!(matches!(
                sanitize_compile_command(&command, Path::new("clang"), ClangLanguage::C),
                Err(SanitizedCommandError::UnsafeArgument(_))
            ));
        }
    }
}
