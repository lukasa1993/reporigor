use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::{ClangLanguage, CompileCommand};

const SEPARATE_OUTPUT_FLAGS: &str = r"-o --output /o -as-secure-log-file -analyzer-dump-egraph -gen-cdb-fragment-path -dependency-dot -dsym-dir -dumpdir -MF -MT -MQ -MJ -dependency-file --dependency-file -diagnostic-log-file -fapinotes-cache-path -fcrash-diagnostics-dir -fmodules-cache-path -fmodules-user-build-path -header-include-file -index-store-path -index-unit-output-path -module-dependency-dir -opt-record-file -serialize-diagnostics --serialize-diagnostics -serialize-diagnostic-file -split-dwarf-file -split-dwarf-output -stack-usage-file -stats-file --symbol-graph-dir -foptimization-record-file";
const EXACT_OUTPUT_FLAGS: &str = r"-c -S -E --analyze -analyze --precompile -extract-api -rewrite-legacy-objc -rewrite-objc -via-file-asm -gen-reproducer -fmodule-output -fcrash-diagnostics -fno-crash-diagnostics -fno-temp-file -fprofile-arcs -fsave-optimization-record -fstack-usage -ftest-coverage -save-temps --save-temps -save-stats -ftime-trace -index-record-codegen-name -index-store-compress -stats-file-append --emit-extension-symbol-graphs --gpu-bundle-output -fdiagnostics-parseable-fixits -fdiagnostics-print-source-range-info -fmemory-profile /c /E /EP /LD /LDd /P /Zs /FA";
const PREFIX_OUTPUT_FLAGS: &str = r"--output= -as-secure-log-file= -analyzer-dump-egraph= -gen-cdb-fragment-path= -dependency-dot= -dsym-dir= -dumpdir= -MF -MT -MQ -MJ -dependency-file= --dependency-file= -diagnostic-log-file= -fapinotes-cache-path= -fcrash-diagnostics-dir= -fmodules-cache-path= -fmodules-user-build-path= -header-include-file= -index-store-path= -index-unit-output-path= -module-dependency-dir= -opt-record-file= -serialize-diagnostics= --serialize-diagnostics= -serialize-diagnostic-file= -split-dwarf-file= -split-dwarf-output= -stack-usage-file= -stats-file= --symbol-graph-dir= -foptimization-record-file= -fmodule-output= -emit- --emit- -dump-depscan-tree= -gen-reproducer= -fcodegen-data-generate -fcrash-diagnostics= -fcs-profile-generate -fmemory-profile= -fproc-stat-report -fprofile-generate -fprofile-instr-generate -fsave-optimization-record= -save-stats= -save-temps= --save-temps= -ftime-trace= /doc /Fa /Fd /Fe /Fi /Fm /Fo /FR /Fr /ifcOutput /module:output /sourceDependencies /Yc";

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
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[must_use]
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
    pub(crate) fn scratch_directory(&self) -> Option<&Path> {
        self.scratch.as_deref().map(tempfile::TempDir::path)
    }
}

pub(crate) fn direct_command(
    program: PathBuf,
    arguments: Vec<String>,
    directory: PathBuf,
) -> SanitizedCommand {
    SanitizedCommand {
        program,
        arguments,
        directory,
        scratch: None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Quote {
    #[default]
    None,
    Single,
    Double,
}

#[derive(Debug, Default)]
struct CommandTokenizer {
    tokens: Vec<String>,
    token: String,
    quote: Quote,
    escaped: bool,
    token_started: bool,
}

impl CommandTokenizer {
    fn consume(&mut self, character: char) -> Result<(), CommandTokenizeError> {
        if self.escaped {
            self.push_literal(character);
            self.escaped = false;
            return Ok(());
        }
        match self.quote {
            Quote::Single => {
                self.consume_single_quoted(character);
                Ok(())
            }
            Quote::Double => self.consume_double_quoted(character),
            Quote::None => self.consume_unquoted(character),
        }
    }

    fn consume_single_quoted(&mut self, character: char) {
        if character == '\'' {
            self.quote = Quote::None;
        } else {
            self.token.push(character);
        }
        self.token_started = true;
    }

    fn consume_double_quoted(&mut self, character: char) -> Result<(), CommandTokenizeError> {
        if character == '"' {
            self.quote = Quote::None;
            self.token_started = true;
            Ok(())
        } else if character == '\\' {
            self.begin_escape();
            Ok(())
        } else {
            self.push_checked(character)
        }
    }

    fn consume_unquoted(&mut self, character: char) -> Result<(), CommandTokenizeError> {
        match character {
            '\'' => self.begin_quote(Quote::Single),
            '"' => self.begin_quote(Quote::Double),
            '\\' => self.begin_escape(),
            ' ' | '\t' => self.finish_token(),
            _ => return self.push_checked(character),
        }
        Ok(())
    }

    fn push_checked(&mut self, character: char) -> Result<(), CommandTokenizeError> {
        Self::reject_shell_character(character)?;
        self.push_literal(character);
        Ok(())
    }

    fn reject_shell_character(character: char) -> Result<(), CommandTokenizeError> {
        const SHELL_SYNTAX: &[char] = &['\n', '\r', '|', '&', ';', '<', '>', '$', '`', '(', ')'];
        if SHELL_SYNTAX.contains(&character) {
            Err(CommandTokenizeError::ShellSyntax { token: character })
        } else {
            Ok(())
        }
    }

    fn begin_quote(&mut self, quote: Quote) {
        self.quote = quote;
        self.token_started = true;
    }

    fn begin_escape(&mut self) {
        self.escaped = true;
        self.token_started = true;
    }

    fn push_literal(&mut self, character: char) {
        self.token.push(character);
        self.token_started = true;
    }

    fn finish_token(&mut self) {
        if self.token_started {
            self.tokens.push(std::mem::take(&mut self.token));
            self.token_started = false;
        }
    }

    fn finish(mut self) -> Result<Vec<String>, CommandTokenizeError> {
        self.validate_complete()?;
        self.finish_token();
        if self.tokens.is_empty() {
            Err(CommandTokenizeError::Empty)
        } else {
            Ok(self.tokens)
        }
    }

    fn validate_complete(&self) -> Result<(), CommandTokenizeError> {
        if self.escaped {
            return Err(CommandTokenizeError::IncompleteEscape);
        }
        quote_error(self.quote).map_or(Ok(()), Err)
    }
}

fn quote_error(quote: Quote) -> Option<CommandTokenizeError> {
    match quote {
        Quote::Single => Some(CommandTokenizeError::UnterminatedQuote("single")),
        Quote::Double => Some(CommandTokenizeError::UnterminatedQuote("double")),
        Quote::None => None,
    }
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

    let mut tokenizer = CommandTokenizer::default();
    for character in command.chars() {
        tokenizer.consume(character)?;
    }
    tokenizer.finish()
}

/// Remove flags that select a compile/output mode or write dependency and
/// diagnostics artifacts. The source path, include paths, definitions, target,
/// language, and other semantic flags remain unchanged.
#[must_use]
pub fn strip_output_and_dependency_flags(arguments: &[String]) -> Vec<String> {
    let mut result = Vec::with_capacity(arguments.len());
    let mut index = 0;
    while index < arguments.len() {
        index = index.saturating_add(strip_argument(arguments, index, &mut result));
    }
    result
}

fn strip_argument(arguments: &[String], index: usize, result: &mut Vec<String>) -> usize {
    let argument = &arguments[index];
    if flag_takes_separate_value(argument) {
        return 2;
    }
    if is_removable_output_flag(argument) {
        return 1;
    }
    if let Some(span) = forwarded_output_span(arguments, index) {
        return span;
    }
    result.push(argument.clone());
    1
}

fn is_removable_output_flag(argument: &str) -> bool {
    is_joined_output_flag(argument) || is_dependency_mode_flag(argument) || is_output_mode_flag(argument)
}

// Clang's driver forwards some output flags through `-Xclang`. Remove the
// complete forwarded pair while preserving unrelated frontend arguments.
fn forwarded_output_span(arguments: &[String], index: usize) -> Option<usize> {
    if arguments.get(index)?.as_str() != "-Xclang" {
        return None;
    }
    let forwarded = arguments.get(index.saturating_add(1))?;
    if flag_takes_separate_value(forwarded) {
        return Some(forwarded_value_span(&arguments[index.saturating_add(2)..]));
    }
    is_removable_output_flag(forwarded).then_some(2)
}

fn forwarded_value_span(tail: &[String]) -> usize {
    match tail {
        [marker, _, ..] if marker == "-Xclang" => 4,
        [_, ..] => 3,
        [] => 2,
    }
}

fn flag_takes_separate_value(argument: &str) -> bool {
    listed_output_flag(SEPARATE_OUTPUT_FLAGS, argument)
}

fn is_joined_output_flag(argument: &str) -> bool {
    crate::word_list_any(PREFIX_OUTPUT_FLAGS, |prefix| argument.starts_with(prefix))
        || is_short_output_flag(argument)
}

fn is_short_output_flag(argument: &str) -> bool {
    const SEMANTIC_O_FLAGS: &str = r"-objc -object -offload -omit -openmp -opt";
    argument.starts_with("-o")
        && argument.len() > 2
        && !crate::word_list_any(SEMANTIC_O_FLAGS, |semantic| argument.starts_with(semantic))
}

fn is_dependency_mode_flag(argument: &str) -> bool {
    listed_output_flag(r"-M -MM -MD -MMD -MG -MP", argument)
}

fn is_output_mode_flag(argument: &str) -> bool {
    listed_output_flag(EXACT_OUTPUT_FLAGS, argument)
}

fn listed_output_flag(list: &str, argument: &str) -> bool {
    crate::word_list_contains(list, argument)
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
    crate::word_list_any("clang clang++ gcc g++ cc c++ icc icx icpx", |driver| {
        stem == driver || stem.starts_with(&format!("{driver}-")) || stem.ends_with(&format!("-{driver}"))
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
    is_direct_escape(argument)
        || is_frontend_forwarding(argument)
        || crate::word_list_any(
            r"--config --config-system-dir --config-user-dir -multi-lib-config",
            |flag| is_flag_or_joined_value(argument, flag),
        )
        || crate::word_list_any(r"-fdepscan -fcas -fcache- -fexperimental-cache", |prefix| {
            argument.starts_with(prefix)
        })
        || is_plugin_route(argument)
}

fn is_direct_escape(argument: &str) -> bool {
    // Response files are expanded by the driver before normal option parsing.
    // `/link` forwards the remaining clang-cl argv to a linker.
    argument.starts_with('@') || argument.eq_ignore_ascii_case("/link")
}

fn is_frontend_forwarding(argument: &str) -> bool {
    const EXACT: &str = r"-Xanalyzer -Xclang -Xclangas -Xpreprocessor -mllvm";
    const PREFIXES: &str = r"-Xanalyzer= -Xclang= -Xclangas= -Xpreprocessor= -mllvm= -Wp,";
    listed_output_flag(EXACT, argument)
        || crate::word_list_any(PREFIXES, |prefix| argument.starts_with(prefix))
        || argument.starts_with("-cc1")
        || argument
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/clang:"))
}

fn is_plugin_route(argument: &str) -> bool {
    const FLAGS: &str = r"-load --load -load-pass-plugin -plugin -add-plugin -fplugin -fpass-plugin --hipspv-pass-plugin -fcas-plugin-path -fcas-plugin-option -wrapper";
    crate::word_list_any(FLAGS, |flag| is_flag_or_joined_value(argument, flag))
        || argument.starts_with("-plugin-arg-")
        || argument.starts_with("-fplugin-arg-")
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
    use crate::test_support::{compile_command, temp_dir};

    mod tokenizer {
        use super::*;

        fn assert_tokenize_error(command: &str, expected: CommandTokenizeError) {
            assert_eq!(tokenize_command(command), Err(expected));
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
            assert_tokenize_error(
                "clang a.c && touch marker",
                CommandTokenizeError::ShellSyntax { token: '&' },
            );
            assert_tokenize_error(
                "clang $(malicious)",
                CommandTokenizeError::ShellSyntax { token: '$' },
            );
            assert_tokenize_error(
                "clang 'unterminated",
                CommandTokenizeError::UnterminatedQuote("single"),
            );
        }

        #[test]
        fn tokenizer_reports_each_terminal_state_and_unquoted_transition() {
            assert_eq!(
                tokenize_command("\talpha\\ beta \"gamma\" 'delta'"),
                Ok(["alpha beta", "gamma", "delta"].map(ToString::to_string).to_vec())
            );
            assert_tokenize_error(" \t ", CommandTokenizeError::Empty);
            assert_tokenize_error("clang\0file.c", CommandTokenizeError::Nul);
            assert_tokenize_error("clang file.c\\", CommandTokenizeError::IncompleteEscape);
            assert_tokenize_error(
                "clang \"unterminated",
                CommandTokenizeError::UnterminatedQuote("double"),
            );
            assert_tokenize_error("clang\nfile.c", CommandTokenizeError::ShellSyntax { token: '\n' });
        }
    }

    mod output_flags {
        use super::*;

        #[test]
        fn strips_output_dependency_and_forwarded_side_effects() {
            let stripped = |values: &[&str]| {
                strip_output_and_dependency_flags(&values.iter().map(ToString::to_string).collect::<Vec<_>>())
            };
            let assert_stripped = |input: &str, expected: &[&str]| {
                assert_eq!(stripped(&crate::fixture_words(input)), expected);
            };
            assert_stripped(
                "-Iinclude -MD -MF main.d -MTmain.o -o main.o -c src/main.c -DVALUE=1",
                &["-Iinclude", "src/main.c", "-DVALUE=1"],
            );
            assert_stripped("-Xclang -MF -Xclang dep.d keep.c", &["keep.c"]);
            assert_stripped("-Xclang -MF dep.d keep.c", &["keep.c"]);
            assert!(stripped(&crate::fixture_words("-Xclang -MF")).is_empty());
            assert_stripped("-Xclang -MFdep.d keep.c", &["keep.c"]);
            assert_stripped(
                "-Xclang -fsyntax-only keep.c",
                &["-Xclang", "-fsyntax-only", "keep.c"],
            );
        }
    }

    mod sanitization {
        use super::*;

        fn command(arguments: &[&str]) -> CompileCommand {
            compile_command(Path::new("/project"), "src/main.c", arguments)
        }

        fn command_line(value: &str) -> CompileCommand {
            command(&value.split_ascii_whitespace().collect::<Vec<_>>())
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

        fn assert_invocation_change(base: &SanitizedCommand, change: impl FnOnce(&mut SanitizedCommand)) {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(*base, changed);
        }

        #[test]
        fn sanitized_command_equality_observes_every_invocation_field() {
            let base = direct_command(
                PathBuf::from("clang"),
                vec!["source.c".to_string()],
                PathBuf::from("/project"),
            );
            assert_eq!(base, base.clone());

            assert_invocation_change(&base, |changed| changed.program = PathBuf::from("clang++"));
            assert_invocation_change(&base, |changed| changed.arguments.push("-Wall".to_string()));
            assert_invocation_change(&base, |changed| changed.directory = PathBuf::from("/other"));
            let temp = temp_dir();
            let with_scratch = SanitizedCommand {
                scratch: Some(Arc::new(temp)),
                ..base.clone()
            };
            assert_ne!(base, with_scratch);
        }

        #[test]
        fn sanitizes_wrapped_command_and_forces_language() {
            let command = command(&crate::fixture_words(
                "ccache /usr/bin/clang++ -std=c++20 -xobjective-c -c src/main.mm -o main.o",
            ));
            let sanitized =
                sanitize_compile_command(&command, Path::new("/usr/bin/clang"), ClangLanguage::ObjectiveCpp)
                    .unwrap_or_else(|error| panic!("sanitize: {error}"));
            assert_eq!(sanitized.program, Path::new("/usr/bin/clang"));
            assert_eq!(
                semantic_arguments(&sanitized, "objective-c++"),
                ["-std=c++20", "src/main.mm"]
            );
        }

        #[test]
        fn language_flag_removal_handles_joined_separate_and_missing_values() {
            let mut arguments = ["-xc++", "-x", "c", "source.c"].map(ToString::to_string).to_vec();
            remove_language_flags(&mut arguments);
            assert_eq!(arguments, ["source.c"]);

            let mut missing_value = vec!["source.c".to_string(), "-x".to_string()];
            remove_language_flags(&mut missing_value);
            assert_eq!(missing_value, ["source.c"]);
        }

        fn assert_unsafe(arguments: &str) {
            let command = command_line(arguments);
            assert!(matches!(
                sanitize_compile_command(&command, Path::new("clang"), ClangLanguage::C),
                Err(SanitizedCommandError::UnsafeArgument(_))
            ));
        }

        fn assert_unsafe_commands(commands: &str) {
            commands.lines().for_each(assert_unsafe);
        }

        fn assert_semantics(
            arguments: &str,
            language: ClangLanguage,
            name: &str,
            expected: &str,
        ) -> SanitizedCommand {
            let command = command_line(arguments);
            let sanitized = sanitize_compile_command(&command, Path::new("clang"), language)
                .unwrap_or_else(|error| panic!("sanitize: {error}"));
            assert_eq!(
                semantic_arguments(&sanitized, name),
                expected.split_ascii_whitespace().collect::<Vec<_>>()
            );
            sanitized
        }

        #[test]
        fn refuses_all_unsafe_driver_and_frontend_routes() {
            assert_unsafe_commands(
                "clang @flags.rsp main.c
clang --config /tmp/evil.cfg main.c
clang --config=/tmp/evil.cfg main.c
clang --config-system-dir /tmp main.c
clang --config-system-dir=/tmp main.c
clang --config-user-dir /tmp main.c
clang --config-user-dir=/tmp main.c
clang -multi-lib-config /tmp/evil.yaml main.c
clang -multi-lib-config=/tmp/evil.yaml main.c
clang-cl /clang:@flags.rsp main.c
clang-cl /clang:--config=/tmp/evil.cfg main.c",
            );

            assert_unsafe_commands(
                "clang -fplugin /tmp/plugin.so main.c
clang -fplugin=/tmp/plugin.so main.c
clang -fpass-plugin /tmp/pass.so main.c
clang -fpass-plugin=/tmp/pass.so main.c
clang --hipspv-pass-plugin=/tmp/pass.so main.c
clang -load /tmp/plugin.so main.c
clang -load=/tmp/plugin.so main.c
clang -load-pass-plugin=/tmp/pass.so main.c
clang -plugin evil main.c
clang -plugin=evil main.c
clang -add-plugin evil main.c
clang -add-plugin=evil main.c
clang -plugin-arg-evil payload main.c
clang -fplugin-arg-evil-payload main.c
clang -fcas-plugin-path /tmp/cas.so main.c
clang -wrapper /tmp/wrapper main.c",
            );

            assert_unsafe_commands(
                "clang -Xclang -load -Xclang /tmp/plugin.so main.c
clang -Xclang=-load -Xclang=/tmp/plugin.so main.c
clang -Xclang=-plugin -Xclang=evil main.c
clang -Xclang=-add-plugin -Xclang=evil main.c
clang -Xclang -fcas-plugin-path -Xclang /tmp/cas.so main.c
clang -Xclang=-fcas-plugin-path -Xclang=/tmp/cas.so main.c
clang -Xpreprocessor -load -Xpreprocessor /tmp/plugin.so main.c
clang -Wp,-load,/tmp/plugin.so main.c
clang -Wp,@/tmp/flags.rsp main.c
clang --analyze -Xanalyzer -analyzer-dump-egraph main.c
clang -mllvm -load=/tmp/pass.so main.c
clang -Xclangas=-load main.c
clang -cc1 -load /tmp/plugin.so main.c
clang-cl /clang:-fplugin=/tmp/plugin.so main.c",
            );
        }

        #[test]
        fn preserves_benign_driver_semantics_without_substring_false_positives() {
            assert_semantics(
            "clang -std=c17 --target=aarch64-unknown-linux-gnu --sysroot=/sdk -Iinclude -isystem vendor/include -include config.h -DPLUGIN_NAME=-load -Wno-pass-failed -fmodules src/plugin-config.c -MD -MF main.d -c -o main.o",
            ClangLanguage::C,
            "c",
            "-std=c17 --target=aarch64-unknown-linux-gnu --sysroot=/sdk -Iinclude -isystem vendor/include -include config.h -DPLUGIN_NAME=-load -Wno-pass-failed -fmodules src/plugin-config.c",
        );
        }

        #[test]
        fn strips_adversarial_write_destinations_and_uses_owned_scratch() {
            let sanitized = assert_semantics(
            "clang -Iinclude -fmodules -fmodules-cache-path=/project/attacker-cache -fmodule-output=/project/attacker.pcm -fcrash-diagnostics=all -fcrash-diagnostics-dir=/project/crash -gen-reproducer=always -gen-cdb-fragment-path /project/cdb -serialize-diagnostics /project/diagnostics.dia -index-store-path /project/index -foptimization-record-file=/project/remarks.yaml -fsave-optimization-record -ftime-trace=/project/trace.json -fproc-stat-report=/project/stats.json -fprofile-instr-generate=/project/default.profraw -fstack-usage -emit-symbol-graph --symbol-graph-dir=/project/symbols -MD -MF /project/main.d -c -o /project/main.o src/main.c",
            ClangLanguage::C,
            "c",
            "-Iinclude -fmodules src/main.c",
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
                assert_unsafe(&format!("clang {unsafe_argument} main.c"));
            }
        }

        #[test]
        fn rejects_each_raw_unsafe_argument() {
            for unsafe_argument in ["@flags.rsp", "-fplugin=/tmp/plugin.so", "-load"] {
                assert_unsafe(&format!("clang {unsafe_argument} main.c"));
            }
        }
    }
}
