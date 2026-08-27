use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use reporigor_core::Language;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::{tokenize_command, CommandTokenizeError};

const DATABASE_NAME: &str = "compile_commands.json";
const MAX_DATABASE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DISCOVERY_ENTRIES: usize = 4_096;
const MAX_DATABASE_ENTRIES: usize = 100_000;
const MAX_ARGUMENTS_PER_ENTRY: usize = 4_096;
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_BYTES_PER_ENTRY: usize = 1024 * 1024;
const MAX_PATH_FIELD_BYTES: usize = 64 * 1024;

/// Errors raised while discovering or parsing an existing compilation
/// database. Discovery never attempts to generate one.
#[derive(Debug, Error)]
pub enum ClangAdapterError {
    #[error("project path {path} does not exist or is not a directory")]
    InvalidRoot { path: String },
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    Parse { path: String, message: String },
    #[error("invalid entry {index} in {path}: {message}")]
    InvalidEntry {
        path: String,
        index: usize,
        message: String,
    },
    #[error("failed to tokenize entry {index} in {path}: {source}")]
    Tokenize {
        path: String,
        index: usize,
        #[source]
        source: CommandTokenizeError,
    },
}

/// How the command was represented in the JSON database. This retains the
/// exact producer output for provenance even though validation always uses the
/// parsed `arguments` field and never a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOrigin {
    Arguments(Vec<String>),
    Command(String),
}

/// One parsed compilation-database record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileCommand {
    /// Resolved working directory. Relative database directories are resolved
    /// against the database's parent directory.
    pub directory: PathBuf,
    /// Resolved translation-unit path. Relative file paths are resolved against
    /// `directory`.
    pub file: PathBuf,
    /// Parsed argv including the database's original executable/wrappers.
    pub arguments: Vec<String>,
    /// Optional producer-described output path.
    pub output: Option<String>,
    /// Exact original command representation.
    pub origin: CommandOrigin,
}

/// An existing parsed JSON compilation database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationDatabase {
    pub path: PathBuf,
    pub commands: Vec<CompileCommand>,
}

/// Language mode understood by the Clang driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClangLanguage {
    C,
    Cpp,
    ObjectiveC,
    ObjectiveCpp,
}

impl ClangLanguage {
    #[must_use]
    pub const fn clang_name(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::Cpp => "c++",
            Self::ObjectiveC => "objective-c",
            Self::ObjectiveCpp => "objective-c++",
        }
    }

    #[must_use]
    pub const fn core_language(self) -> Language {
        match self {
            Self::C => Language::C,
            Self::Cpp => Language::Cpp,
            Self::ObjectiveC | Self::ObjectiveCpp => Language::ObjectiveC,
        }
    }

    /// Classify a translation unit using the explicit `-x` mode first, then
    /// its extension, and finally the compiler driver's C/C++ flavor.
    #[must_use]
    pub fn classify(command: &CompileCommand) -> Option<Self> {
        match explicit_language(&command.arguments) {
            ExplicitLanguage::Supported(language) => Some(language),
            ExplicitLanguage::Unsupported => None,
            ExplicitLanguage::NotSpecified => {
                language_from_path(&command.file).or_else(|| language_from_driver(&command.arguments))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawCompileCommand {
    directory: String,
    file: String,
    #[serde(default)]
    arguments: Option<LimitedArguments>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug)]
struct LimitedArguments(Vec<String>);

impl<'de> Deserialize<'de> for LimitedArguments {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LimitedArgumentsVisitor;

        impl<'de> Visitor<'de> for LimitedArgumentsVisitor {
            type Value = LimitedArguments;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded compilation argument array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut arguments = Vec::with_capacity(
                    sequence
                        .size_hint()
                        .unwrap_or_default()
                        .min(MAX_ARGUMENTS_PER_ENTRY),
                );
                let mut total_bytes = 0_usize;
                while let Some(argument) = sequence.next_element::<String>()? {
                    if arguments.len() == MAX_ARGUMENTS_PER_ENTRY {
                        return Err(de::Error::custom(format_args!(
                            "argv exceeds the {MAX_ARGUMENTS_PER_ENTRY}-argument limit"
                        )));
                    }
                    if argument.len() > MAX_ARGUMENT_BYTES {
                        return Err(de::Error::custom(format_args!(
                            "argument exceeds the {MAX_ARGUMENT_BYTES}-byte token limit"
                        )));
                    }
                    total_bytes += argument.len();
                    if total_bytes > MAX_ARGUMENT_BYTES_PER_ENTRY {
                        return Err(de::Error::custom(format_args!(
                            "argv exceeds the {MAX_ARGUMENT_BYTES_PER_ENTRY}-byte aggregate limit"
                        )));
                    }
                    arguments.push(argument);
                }
                Ok(LimitedArguments(arguments))
            }
        }

        deserializer.deserialize_seq(LimitedArgumentsVisitor)
    }
}

struct RawCompileCommands(Vec<RawCompileCommand>);

impl<'de> Deserialize<'de> for RawCompileCommands {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawCompileCommandsVisitor;

        impl<'de> Visitor<'de> for RawCompileCommandsVisitor {
            type Value = RawCompileCommands;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of compilation database entries")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut commands =
                    Vec::with_capacity(sequence.size_hint().unwrap_or_default().min(MAX_DATABASE_ENTRIES));
                while let Some(command) = sequence.next_element()? {
                    if commands.len() == MAX_DATABASE_ENTRIES {
                        return Err(de::Error::custom(format_args!(
                            "compilation database exceeds the {MAX_DATABASE_ENTRIES}-entry limit"
                        )));
                    }
                    commands.push(command);
                }
                Ok(RawCompileCommands(commands))
            }
        }

        deserializer.deserialize_seq(RawCompileCommandsVisitor)
    }
}

/// Locate an already-generated compilation database. Root-level databases take
/// precedence, followed by conventional build directories, then an immediate
/// child directory in lexical order. The function performs no build-system
/// invocation or recursive generation.
///
/// # Errors
///
/// Returns an error when `root` is invalid or its directory entries cannot be
/// inspected.
pub fn discover_compilation_database(root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    let root_metadata = fs::metadata(root).map_err(|_| ClangAdapterError::InvalidRoot {
        path: root.display().to_string(),
    })?;
    if root_metadata.is_file() {
        return if root.file_name().is_some_and(|name| name == DATABASE_NAME) {
            canonical_regular_file(root).map(Some)
        } else {
            Err(ClangAdapterError::InvalidRoot {
                path: root.display().to_string(),
            })
        };
    }
    if !root_metadata.is_dir() {
        return Err(ClangAdapterError::InvalidRoot {
            path: root.display().to_string(),
        });
    }
    let canonical_root = canonical_directory(root)?;

    for relative in [
        DATABASE_NAME,
        "build/compile_commands.json",
        ".build/compile_commands.json",
        "out/compile_commands.json",
    ] {
        let candidate = canonical_root.join(relative);
        if let Some(candidate) = automatic_candidate(&candidate, &canonical_root)? {
            return Ok(Some(candidate));
        }
    }

    let mut child_candidates = Vec::new();
    for (index, entry) in fs::read_dir(&canonical_root)
        .map_err(|source| ClangAdapterError::Read {
            path: canonical_root.display().to_string(),
            source,
        })?
        .enumerate()
    {
        if index == MAX_DISCOVERY_ENTRIES {
            return Err(invalid_data_error(
                &canonical_root,
                format!("directory exceeds the {MAX_DISCOVERY_ENTRIES}-entry discovery limit"),
            ));
        }
        let entry = entry.map_err(|source| ClangAdapterError::Read {
            path: canonical_root.display().to_string(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| ClangAdapterError::Read {
                path: entry.path().display().to_string(),
                source,
            })?
            .is_dir()
        {
            let candidate = entry.path().join(DATABASE_NAME);
            if let Some(candidate) = automatic_candidate(&candidate, &canonical_root)? {
                child_candidates.push(candidate);
            }
        }
    }
    child_candidates.sort();
    Ok(child_candidates.into_iter().next())
}

fn canonical_file(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    path.canonicalize().map_err(|source| ClangAdapterError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn canonical_regular_file(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    let link_metadata = fs::symlink_metadata(path).map_err(|source| ClangAdapterError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if link_metadata.file_type().is_symlink() {
        return Err(invalid_data_error(
            path,
            "symbolic links are not accepted for compilation databases",
        ));
    }
    if !link_metadata.is_file() {
        return Err(invalid_data_error(path, "expected a regular file"));
    }
    let canonical = canonical_file(path)?;
    let metadata = fs::metadata(&canonical).map_err(|source| ClangAdapterError::Read {
        path: canonical.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(invalid_data_error(&canonical, "expected a regular file"));
    }
    Ok(canonical)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    let canonical = canonical_file(path)?;
    let metadata = fs::metadata(&canonical).map_err(|source| ClangAdapterError::Read {
        path: canonical.display().to_string(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ClangAdapterError::InvalidRoot {
            path: path.display().to_string(),
        });
    }
    Ok(canonical)
}

fn automatic_candidate(path: &Path, canonical_root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ClangAdapterError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(invalid_data_error(
            path,
            "symbolic links are not accepted for compilation databases",
        ));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let canonical = canonical_regular_file(path)?;
    if !canonical.starts_with(canonical_root) {
        return Err(invalid_data_error(
            path,
            format!(
                "automatically discovered database resolves outside project root {}",
                canonical_root.display()
            ),
        ));
    }
    Ok(Some(canonical))
}

fn invalid_data_error(path: &Path, message: impl Into<String>) -> ClangAdapterError {
    ClangAdapterError::Read {
        path: path.display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()),
    }
}

/// Parse a JSON compilation database and preserve each record's exact command
/// provenance. `arguments` and `command` are mutually exclusive per the
/// standard; ambiguous records are rejected.
///
/// # Errors
///
/// Returns an error when the database cannot be read, contains malformed JSON,
/// has invalid entries, or relies on unsupported shell syntax.
pub fn load_database(path: &Path) -> Result<CompilationDatabase, ClangAdapterError> {
    let path = canonical_regular_file(path)?;
    let contents = read_database_bytes(&path)?;
    let RawCompileCommands(raw) =
        serde_json::from_slice(&contents).map_err(|error| ClangAdapterError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    let database_directory = path.parent().unwrap_or_else(|| Path::new("."));
    let commands = raw
        .into_iter()
        .enumerate()
        .map(|(index, entry)| compile_command_from_raw(&path, database_directory, index, entry))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CompilationDatabase { path, commands })
}

fn read_database_bytes(path: &Path) -> Result<Vec<u8>, ClangAdapterError> {
    let file = File::open(path).map_err(|source| ClangAdapterError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ClangAdapterError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(invalid_data_error(path, "expected a regular file"));
    }
    if metadata.len() > MAX_DATABASE_BYTES {
        return Err(invalid_data_error(
            path,
            format!("database exceeds the {MAX_DATABASE_BYTES}-byte limit"),
        ));
    }
    let initial_capacity = usize::try_from(metadata.len())
        .map_err(|_| invalid_data_error(path, "database size cannot be represented on this platform"))?;
    let maximum_capacity = usize::try_from(MAX_DATABASE_BYTES)
        .map_err(|_| invalid_data_error(path, "database limit cannot be represented on this platform"))?;
    let mut contents = Vec::with_capacity(initial_capacity.saturating_add(1));
    file.take(MAX_DATABASE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|source| ClangAdapterError::Read {
            path: path.display().to_string(),
            source,
        })?;
    if contents.len() > maximum_capacity {
        return Err(invalid_data_error(
            path,
            format!("database exceeds the {MAX_DATABASE_BYTES}-byte limit while reading"),
        ));
    }
    Ok(contents)
}

fn compile_command_from_raw(
    path: &Path,
    database_directory: &Path,
    index: usize,
    entry: RawCompileCommand,
) -> Result<CompileCommand, ClangAdapterError> {
    validate_entry_fields(path, index, &entry)?;
    let (arguments, origin) = match (entry.arguments, entry.command) {
        (Some(LimitedArguments(arguments)), None) if !arguments.is_empty() => {
            validate_arguments(path, index, &arguments)?;
            (arguments.clone(), CommandOrigin::Arguments(arguments))
        }
        (None, Some(command)) => {
            if command.len() > MAX_COMMAND_BYTES {
                return Err(invalid_entry(
                    path,
                    index,
                    format!("command exceeds the {MAX_COMMAND_BYTES}-byte limit"),
                ));
            }
            let arguments = tokenize_command(&command).map_err(|source| ClangAdapterError::Tokenize {
                path: path.display().to_string(),
                index,
                source,
            })?;
            validate_arguments(path, index, &arguments)?;
            (arguments, CommandOrigin::Command(command))
        }
        (Some(_), Some(_)) => {
            return Err(invalid_entry(
                path,
                index,
                "both arguments and command are present",
            ));
        }
        (Some(_), None) => return Err(invalid_entry(path, index, "arguments is empty")),
        (None, None) => {
            return Err(invalid_entry(
                path,
                index,
                "neither arguments nor command is present",
            ));
        }
    };
    if entry.directory.is_empty() || entry.file.is_empty() {
        return Err(invalid_entry(path, index, "directory and file must be non-empty"));
    }
    let raw_directory = Path::new(&entry.directory);
    let directory = if raw_directory.is_absolute() {
        normalize_path(raw_directory)
    } else {
        normalize_path(&database_directory.join(raw_directory))
    };
    let raw_file = Path::new(&entry.file);
    let file = if raw_file.is_absolute() {
        normalize_path(raw_file)
    } else {
        normalize_path(&directory.join(raw_file))
    };
    Ok(CompileCommand {
        directory,
        file,
        arguments,
        output: entry.output,
        origin,
    })
}

fn validate_entry_fields(
    path: &Path,
    index: usize,
    entry: &RawCompileCommand,
) -> Result<(), ClangAdapterError> {
    for (name, value) in [
        ("directory", entry.directory.as_str()),
        ("file", entry.file.as_str()),
    ] {
        if value.len() > MAX_PATH_FIELD_BYTES {
            return Err(invalid_entry(
                path,
                index,
                format!("{name} exceeds the {MAX_PATH_FIELD_BYTES}-byte limit"),
            ));
        }
    }
    if entry
        .output
        .as_ref()
        .is_some_and(|output| output.len() > MAX_PATH_FIELD_BYTES)
    {
        return Err(invalid_entry(
            path,
            index,
            format!("output exceeds the {MAX_PATH_FIELD_BYTES}-byte limit"),
        ));
    }
    Ok(())
}

fn validate_arguments(path: &Path, index: usize, arguments: &[String]) -> Result<(), ClangAdapterError> {
    if arguments.len() > MAX_ARGUMENTS_PER_ENTRY {
        return Err(invalid_entry(
            path,
            index,
            format!("argv exceeds the {MAX_ARGUMENTS_PER_ENTRY}-argument limit"),
        ));
    }
    let mut total_bytes = 0_usize;
    for argument in arguments {
        if argument.len() > MAX_ARGUMENT_BYTES {
            return Err(invalid_entry(
                path,
                index,
                format!("argument exceeds the {MAX_ARGUMENT_BYTES}-byte token limit"),
            ));
        }
        total_bytes = total_bytes.saturating_add(argument.len());
        if total_bytes > MAX_ARGUMENT_BYTES_PER_ENTRY {
            return Err(invalid_entry(
                path,
                index,
                format!("argv exceeds the {MAX_ARGUMENT_BYTES_PER_ENTRY}-byte aggregate limit"),
            ));
        }
    }
    Ok(())
}

fn invalid_entry(path: &Path, index: usize, message: impl Into<String>) -> ClangAdapterError {
    ClangAdapterError::InvalidEntry {
        path: path.display().to_string(),
        index,
        message: message.into(),
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplicitLanguage {
    NotSpecified,
    Supported(ClangLanguage),
    Unsupported,
}

fn explicit_language(arguments: &[String]) -> ExplicitLanguage {
    let mut selected = ExplicitLanguage::NotSpecified;
    let mut index = 0;
    while index < arguments.len() {
        let value = if arguments[index] == "-x" {
            index += 1;
            match arguments.get(index) {
                Some(value) => Some(value.as_str()),
                None => return ExplicitLanguage::Unsupported,
            }
        } else {
            arguments[index]
                .strip_prefix("-x")
                .filter(|value| !value.is_empty())
        };
        if let Some(value) = value {
            selected = if value == "none" {
                ExplicitLanguage::NotSpecified
            } else {
                parse_language_name(value).map_or(ExplicitLanguage::Unsupported, ExplicitLanguage::Supported)
            };
        }
        index += 1;
    }
    selected
}

fn parse_language_name(value: &str) -> Option<ClangLanguage> {
    match value.to_ascii_lowercase().as_str() {
        "c" | "cpp-output" | "c-header" => Some(ClangLanguage::C),
        "c++" | "c++-cpp-output" | "c++-header" => Some(ClangLanguage::Cpp),
        "objective-c" | "objective-c-cpp-output" | "objective-c-header" => Some(ClangLanguage::ObjectiveC),
        "objective-c++" | "objective-c++-cpp-output" | "objective-c++-header" => {
            Some(ClangLanguage::ObjectiveCpp)
        }
        _ => None,
    }
}

fn language_from_path(path: &Path) -> Option<ClangLanguage> {
    let extension = path.extension()?.to_str()?;
    match extension {
        "c" | "i" => Some(ClangLanguage::C),
        "C" | "cc" | "cp" | "cpp" | "cxx" | "c++" | "ii" | "CPP" | "CXX" => Some(ClangLanguage::Cpp),
        "m" | "mi" => Some(ClangLanguage::ObjectiveC),
        "mm" | "mii" | "M" => Some(ClangLanguage::ObjectiveCpp),
        _ => None,
    }
}

fn language_from_driver(arguments: &[String]) -> Option<ClangLanguage> {
    arguments.iter().find_map(|argument| {
        let basename = Path::new(argument).file_name()?.to_str()?.to_ascii_lowercase();
        if basename.contains("clang++") || basename == "g++" || basename == "c++" {
            Some(ClangLanguage::Cpp)
        } else if basename.contains("clang") || basename == "gcc" || basename == "cc" {
            Some(ClangLanguage::C)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use tempfile::tempdir;

    use super::*;

    fn compile_command(file: &str, arguments: &[&str]) -> CompileCommand {
        CompileCommand {
            directory: PathBuf::from("/project"),
            file: PathBuf::from(file),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            output: None,
            origin: CommandOrigin::Arguments(arguments.iter().map(ToString::to_string).collect()),
        }
    }

    fn expect_adapter_error<T: std::fmt::Debug>(result: Result<T, ClangAdapterError>) -> ClangAdapterError {
        match result {
            Ok(value) => panic!("expected adapter error, got {value:?}"),
            Err(error) => error,
        }
    }

    #[test]
    fn discovers_root_before_build_and_never_generates_a_database() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        assert_eq!(
            discover_compilation_database(temp.path()).unwrap_or_else(|error| panic!("discover: {error}")),
            None
        );
        assert!(!temp.path().join(DATABASE_NAME).exists());

        fs::create_dir(temp.path().join("build")).unwrap_or_else(|error| panic!("create build: {error}"));
        fs::write(temp.path().join("build").join(DATABASE_NAME), "[]")
            .unwrap_or_else(|error| panic!("write build db: {error}"));
        let build = discover_compilation_database(temp.path())
            .unwrap_or_else(|error| panic!("discover build: {error}"))
            .unwrap_or_else(|| panic!("missing build database"));
        assert!(build.ends_with("build/compile_commands.json"));

        fs::write(temp.path().join(DATABASE_NAME), "[]")
            .unwrap_or_else(|error| panic!("write root db: {error}"));
        let root = discover_compilation_database(temp.path())
            .unwrap_or_else(|error| panic!("discover root: {error}"))
            .unwrap_or_else(|| panic!("missing root database"));
        assert_eq!(
            root,
            temp.path()
                .join(DATABASE_NAME)
                .canonicalize()
                .unwrap_or_else(|error| panic!("canonical database: {error}"))
        );
    }

    #[test]
    fn loads_arguments_and_command_records_with_provenance() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = temp.path().join(DATABASE_NAME);
        fs::write(
            &database,
            r#"[
                {"directory":".","file":"src/a.c","arguments":["clang","-c","src/a.c"]},
                {"directory":".","file":"src/b.cpp","command":"clang++ -c 'src/b.cpp'","output":"b.o"}
            ]"#,
        )
        .unwrap_or_else(|error| panic!("write db: {error}"));

        let loaded = load_database(&database).unwrap_or_else(|error| panic!("load: {error}"));
        assert_eq!(loaded.commands.len(), 2);
        assert!(matches!(loaded.commands[0].origin, CommandOrigin::Arguments(_)));
        assert_eq!(
            loaded.commands[1].origin,
            CommandOrigin::Command("clang++ -c 'src/b.cpp'".to_string())
        );
        assert_eq!(loaded.commands[1].output.as_deref(), Some("b.o"));
        assert_eq!(
            loaded.commands[0].file,
            temp.path()
                .canonicalize()
                .unwrap_or_else(|error| panic!("canonical tempdir: {error}"))
                .join("src/a.c")
        );
    }

    #[test]
    fn rejects_sparse_database_larger_than_the_file_limit() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = temp.path().join(DATABASE_NAME);
        let file = File::create(&database).unwrap_or_else(|error| panic!("create sparse db: {error}"));
        file.set_len(MAX_DATABASE_BYTES + 1)
            .unwrap_or_else(|error| panic!("size sparse db: {error}"));

        let error = expect_adapter_error(load_database(&database));
        assert!(error.to_string().contains("67108864-byte limit"));
    }

    #[test]
    fn rejects_non_regular_database_paths() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

        let error = expect_adapter_error(load_database(temp.path()));
        assert!(error.to_string().contains("expected a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_regular_socket_without_opening_it_as_a_database() {
        use std::os::unix::net::UnixListener;

        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let socket = temp.path().join(DATABASE_NAME);
        let _listener = UnixListener::bind(&socket).unwrap_or_else(|error| panic!("bind socket: {error}"));

        let error = expect_adapter_error(load_database(&socket));
        assert!(error.to_string().contains("expected a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_database_symlinks_and_out_of_root_intermediate_symlinks() {
        use std::os::unix::fs::symlink;

        let project = tempdir().unwrap_or_else(|error| panic!("project tempdir: {error}"));
        let in_root_target = project.path().join("actual-database.json");
        fs::write(&in_root_target, "[]").unwrap_or_else(|error| panic!("write in-root db: {error}"));
        let candidate = project.path().join(DATABASE_NAME);
        symlink(&in_root_target, &candidate).unwrap_or_else(|error| panic!("link in-root db: {error}"));

        let discovery_error = expect_adapter_error(discover_compilation_database(project.path()));
        assert!(discovery_error
            .to_string()
            .contains("symbolic links are not accepted"));
        let load_error = expect_adapter_error(load_database(&candidate));
        assert!(load_error.to_string().contains("symbolic links are not accepted"));

        fs::remove_file(&candidate).unwrap_or_else(|error| panic!("remove in-root symlink: {error}"));
        let external = tempdir().unwrap_or_else(|error| panic!("external tempdir: {error}"));
        let external_build = external.path().join("external-build");
        fs::create_dir(&external_build).unwrap_or_else(|error| panic!("create external build: {error}"));
        let external_database = external_build.join(DATABASE_NAME);
        fs::write(&external_database, "[]").unwrap_or_else(|error| panic!("write external db: {error}"));
        symlink(&external_build, project.path().join("build"))
            .unwrap_or_else(|error| panic!("link external build: {error}"));

        let error = expect_adapter_error(discover_compilation_database(project.path()));
        assert!(error.to_string().contains("resolves outside project root"));
    }

    #[test]
    fn bounds_immediate_child_enumeration() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        for index in 0..=MAX_DISCOVERY_ENTRIES {
            File::create(temp.path().join(format!("entry-{index}")))
                .unwrap_or_else(|error| panic!("create discovery entry {index}: {error}"));
        }

        let error = expect_adapter_error(discover_compilation_database(temp.path()));
        assert!(error.to_string().contains("4096-entry discovery limit"));
    }

    #[test]
    fn rejects_database_entry_count_above_limit_while_deserializing() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = temp.path().join(DATABASE_NAME);
        let entry = r#"{"directory":".","file":"a.c","arguments":["clang"]}"#;
        let mut contents = String::with_capacity(entry.len() * (MAX_DATABASE_ENTRIES + 1));
        contents.push('[');
        for index in 0..=MAX_DATABASE_ENTRIES {
            if index != 0 {
                contents.push(',');
            }
            contents.push_str(entry);
        }
        contents.push(']');
        assert!(u64::try_from(contents.len()).unwrap_or(u64::MAX) < MAX_DATABASE_BYTES);
        fs::write(&database, contents).unwrap_or_else(|error| panic!("write entry-heavy db: {error}"));

        let error = expect_adapter_error(load_database(&database));
        assert!(error.to_string().contains("exceeds the 100000-entry limit"));
    }

    #[test]
    fn bounds_command_and_argument_payloads() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = temp.path().join(DATABASE_NAME);
        let command = "x".repeat(MAX_COMMAND_BYTES + 1);
        fs::write(
            &database,
            serde_json::json!([{"directory": ".", "file": "a.c", "command": command}]).to_string(),
        )
        .unwrap_or_else(|error| panic!("write command-heavy db: {error}"));
        assert!(expect_adapter_error(load_database(&database))
            .to_string()
            .contains("command exceeds the 1048576-byte limit"));

        let oversized_token = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        fs::write(
            &database,
            serde_json::json!([{
                "directory": ".",
                "file": "a.c",
                "arguments": ["clang", oversized_token]
            }])
            .to_string(),
        )
        .unwrap_or_else(|error| panic!("write token-heavy db: {error}"));
        assert!(expect_adapter_error(load_database(&database))
            .to_string()
            .contains("65536-byte token limit"));

        let aggregate_arguments = vec!["x".repeat(MAX_ARGUMENT_BYTES); 17];
        fs::write(
            &database,
            serde_json::json!([{
                "directory": ".",
                "file": "a.c",
                "arguments": aggregate_arguments
            }])
            .to_string(),
        )
        .unwrap_or_else(|error| panic!("write aggregate argv-heavy db: {error}"));
        assert!(expect_adapter_error(load_database(&database))
            .to_string()
            .contains("1048576-byte aggregate limit"));

        let arguments = vec!["x"; MAX_ARGUMENTS_PER_ENTRY + 1];
        fs::write(
            &database,
            serde_json::json!([{
                "directory": ".",
                "file": "a.c",
                "arguments": arguments
            }])
            .to_string(),
        )
        .unwrap_or_else(|error| panic!("write argv-heavy db: {error}"));
        assert!(expect_adapter_error(load_database(&database))
            .to_string()
            .contains("4096-argument limit"));

        let oversized_path = "x".repeat(MAX_PATH_FIELD_BYTES + 1);
        fs::write(
            &database,
            serde_json::json!([{
                "directory": oversized_path,
                "file": "a.c",
                "arguments": ["clang"]
            }])
            .to_string(),
        )
        .unwrap_or_else(|error| panic!("write path-heavy db: {error}"));
        assert!(expect_adapter_error(load_database(&database))
            .to_string()
            .contains("directory exceeds the 65536-byte limit"));
    }

    #[test]
    fn rejects_ambiguous_or_shell_dependent_commands() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = temp.path().join(DATABASE_NAME);
        fs::write(
            &database,
            r#"[{"directory":".","file":"a.c","command":"clang a.c && touch bad"}]"#,
        )
        .unwrap_or_else(|error| panic!("write db: {error}"));
        assert!(matches!(
            load_database(&database),
            Err(ClangAdapterError::Tokenize { .. })
        ));
    }

    #[test]
    fn classifies_all_clang_language_modes() {
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.c", &["clang", "a.c"])),
            Some(ClangLanguage::C)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.C", &["clang", "a.C"])),
            Some(ClangLanguage::Cpp)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.m", &["clang", "a.m"])),
            Some(ClangLanguage::ObjectiveC)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.mm", &["clang++", "a.mm"])),
            Some(ClangLanguage::ObjectiveCpp)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.c", &["clang", "-x", "objective-c++", "a.c"])),
            Some(ClangLanguage::ObjectiveCpp)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("header.h", &["clang++", "header.h"])),
            Some(ClangLanguage::Cpp)
        );
        assert_eq!(
            ClangLanguage::classify(&compile_command("a.c", &["clang", "-x", "cuda", "a.c"])),
            None
        );
    }
}
