use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::marker::PhantomData;
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
const LANGUAGE_EXTENSIONS: &str =
    "c,i=c;C,cc,cp,cpp,cxx,c++,ii,CPP,CXX=c++;m,mi=objective-c;mm,mii,M=objective-c++";

/// Errors raised while discovering or parsing an existing compilation
/// database. Discovery never attempts to generate one.
#[derive(Debug, Error)]
pub enum ClangAdapterError {
    #[error("project path {path} does not exist or is not a directory")]
    InvalidRoot { path: String },
    #[error("failed to {operation} {path}: {source}")]
    Read {
        operation: &'static str,
        #[source]
        source: std::io::Error,
        path: String,
    },
    #[error("failed to parse {path}: {message}")]
    Parse { message: String, path: String },
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
    /// Exact original command representation.
    pub origin: CommandOrigin,
    /// Optional producer-described output path.
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[doc = "An existing parsed JSON compilation database."]
#[must_use = "use the commands loaded from the compilation database"]
pub struct CompilationDatabase {
    /// Canonical path of the loaded compilation database.
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

struct RawCompileCommands(Vec<RawCompileCommand>);

trait BoundedSequence: Sized {
    type Item;
    type State: Default;

    const DESCRIPTION: &'static str;
    const MAXIMUM: usize;

    fn limit_message() -> String;
    fn validate(value: &Self::Item, state: &mut Self::State) -> Result<(), String>;
    fn from_values(values: Vec<Self::Item>) -> Self;
}

impl BoundedSequence for LimitedArguments {
    type Item = String;
    type State = usize;

    const DESCRIPTION: &'static str = "a bounded compilation argument array";
    const MAXIMUM: usize = MAX_ARGUMENTS_PER_ENTRY;

    fn limit_message() -> String {
        format!("argv exceeds the {MAX_ARGUMENTS_PER_ENTRY}-argument limit")
    }

    fn validate(argument: &String, total_bytes: &mut usize) -> Result<(), String> {
        validate_deserialized_argument(argument, total_bytes)
    }

    fn from_values(values: Vec<String>) -> Self {
        Self(values)
    }
}

impl BoundedSequence for RawCompileCommands {
    type State = ();
    type Item = RawCompileCommand;

    const MAXIMUM: usize = MAX_DATABASE_ENTRIES;
    const DESCRIPTION: &'static str = "an array of compilation database entries";

    fn limit_message() -> String {
        format!("compilation database exceeds the {MAX_DATABASE_ENTRIES}-entry limit")
    }

    fn from_values(values: Vec<RawCompileCommand>) -> Self {
        Self(values)
    }

    fn validate(_: &RawCompileCommand, (): &mut ()) -> Result<(), String> {
        Ok(())
    }
}

macro_rules! impl_bounded_sequence_deserialize {
    ($wrapper:ty) => {
        impl<'de> Deserialize<'de> for $wrapper {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                deserialize_bounded_wrapper(deserializer)
            }
        }
    };
}

impl_bounded_sequence_deserialize!(LimitedArguments);
impl_bounded_sequence_deserialize!(RawCompileCommands);

fn deserialize_bounded_wrapper<'de, D, W>(deserializer: D) -> Result<W, D::Error>
where
    D: Deserializer<'de>,
    W: BoundedSequence,
    W::Item: Deserialize<'de>,
{
    let mut state = W::State::default();
    deserialize_bounded_sequence(
        deserializer,
        W::DESCRIPTION,
        W::MAXIMUM,
        W::limit_message(),
        |value| W::validate(value, &mut state),
    )
    .map(W::from_values)
}

struct BoundedSequenceVisitor<T, F> {
    description: &'static str,
    maximum: usize,
    limit_message: String,
    validate: F,
    marker: PhantomData<T>,
}

impl<'de, T, F> Visitor<'de> for BoundedSequenceVisitor<T, F>
where
    T: Deserialize<'de>,
    F: FnMut(&T) -> Result<(), String>,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.description)
    }

    fn visit_seq<A>(mut self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        collect_bounded_sequence(
            &mut sequence,
            self.maximum,
            self.limit_message,
            &mut self.validate,
        )
    }
}

fn deserialize_bounded_sequence<'de, D, T, F>(
    deserializer: D,
    description: &'static str,
    maximum: usize,
    limit_message: String,
    validate: F,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
    F: FnMut(&T) -> Result<(), String>,
{
    deserializer.deserialize_seq(BoundedSequenceVisitor {
        description,
        maximum,
        limit_message,
        validate,
        marker: PhantomData,
    })
}

fn collect_bounded_sequence<
    'de,
    T: Deserialize<'de>,
    F: FnMut(&T) -> Result<(), String>,
    A: SeqAccess<'de>,
>(
    sequence: &mut A,
    maximum: usize,
    limit_message: String,
    mut validate: F,
) -> Result<Vec<T>, A::Error> {
    let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or_default().min(maximum));
    while let Some(value) = sequence.next_element()? {
        if values.len() == maximum {
            return Err(de::Error::custom(limit_message));
        }
        validate(&value).map_err(de::Error::custom)?;
        values.push(value);
    }
    Ok(values)
}

fn validate_deserialized_argument(argument: &str, total_bytes: &mut usize) -> Result<(), String> {
    if argument.len() > MAX_ARGUMENT_BYTES {
        return Err(format!(
            "argument exceeds the {MAX_ARGUMENT_BYTES}-byte token limit"
        ));
    }
    *total_bytes = total_bytes.saturating_add(argument.len());
    if *total_bytes > MAX_ARGUMENT_BYTES_PER_ENTRY {
        return Err(format!(
            "argv exceeds the {MAX_ARGUMENT_BYTES_PER_ENTRY}-byte aggregate limit"
        ));
    }
    Ok(())
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
    let canonical_root = match discovery_root(root)? {
        DiscoveryRoot::Database(path) => return Ok(Some(path)),
        DiscoveryRoot::Directory(path) => path,
    };
    if let Some(candidate) = conventional_candidate(&canonical_root)? {
        return Ok(Some(candidate));
    }
    child_candidate(&canonical_root)
}

enum DiscoveryRoot {
    Database(PathBuf),
    Directory(PathBuf),
}

fn discovery_root(root: &Path) -> Result<DiscoveryRoot, ClangAdapterError> {
    let root_metadata = fs::metadata(root).map_err(|_| invalid_root(root))?;
    if root_metadata.is_file() {
        return database_root(root);
    }
    if !root_metadata.is_dir() {
        return Err(invalid_root(root));
    }
    canonical_directory(root).map(DiscoveryRoot::Directory)
}

fn database_root(root: &Path) -> Result<DiscoveryRoot, ClangAdapterError> {
    if root.file_name().is_some_and(|name| name == DATABASE_NAME) {
        canonical_regular_file(root).map(DiscoveryRoot::Database)
    } else {
        Err(invalid_root(root))
    }
}

fn conventional_candidate(canonical_root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    for relative in [
        DATABASE_NAME,
        "build/compile_commands.json",
        ".build/compile_commands.json",
        "out/compile_commands.json",
    ] {
        let candidate = canonical_root.join(relative);
        if let Some(candidate) = automatic_candidate(&candidate, canonical_root)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn child_candidate(canonical_root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    let mut child_candidates = Vec::new();
    let entries = child_entries(canonical_root)?;
    for (index, entry) in entries.enumerate() {
        if let Some(candidate) = inspect_child_entry(canonical_root, index, entry)? {
            child_candidates.push(candidate);
        }
    }
    child_candidates.sort();
    Ok(child_candidates.into_iter().next())
}

fn child_entries(root: &Path) -> Result<fs::ReadDir, ClangAdapterError> {
    fs::read_dir(root).map_err(|source| read_error(root, source))
}

fn inspect_child_entry(
    canonical_root: &Path,
    index: usize,
    entry: Result<fs::DirEntry, std::io::Error>,
) -> Result<Option<PathBuf>, ClangAdapterError> {
    if index == MAX_DISCOVERY_ENTRIES {
        return Err(invalid_data_error(
            canonical_root,
            format!("directory exceeds the {MAX_DISCOVERY_ENTRIES}-entry discovery limit"),
        ));
    }
    let entry = entry.map_err(|source| read_error(canonical_root, source))?;
    let file_type = entry
        .file_type()
        .map_err(|source| read_error(&entry.path(), source))?;
    if !file_type.is_dir() {
        return Ok(None);
    }
    automatic_candidate(&entry.path().join(DATABASE_NAME), canonical_root)
}

fn canonical_file(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    io_at_path(path, path.canonicalize())
}

fn canonical_regular_file(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    validate_regular_database_path(path)?;
    let canonical = canonical_file(path)?;
    let metadata = path_metadata(&canonical, |candidate| fs::metadata(candidate))?;
    ensure_regular(&canonical, &metadata)?;
    Ok(canonical)
}

fn validate_regular_database_path(path: &Path) -> Result<(), ClangAdapterError> {
    let metadata = path_metadata(path, |candidate| fs::symlink_metadata(candidate))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid_data_error(
            path,
            "symbolic links are not accepted for compilation databases",
        ));
    }
    ensure_regular(path, &metadata)
}

fn ensure_regular(path: &Path, metadata: &fs::Metadata) -> Result<(), ClangAdapterError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(invalid_data_error(path, "expected a regular file"))
    }
}

fn path_metadata(
    path: &Path,
    read: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
) -> Result<fs::Metadata, ClangAdapterError> {
    io_at_path(path, read(path))
}

fn io_at_path<T>(path: &Path, result: std::io::Result<T>) -> Result<T, ClangAdapterError> {
    result.map_err(|source| read_error(path, source))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ClangAdapterError> {
    let canonical = canonical_file(path)?;
    if path_metadata(&canonical, |candidate| fs::metadata(candidate))?.is_dir() {
        Ok(canonical)
    } else {
        Err(invalid_root(path))
    }
}

fn automatic_candidate(path: &Path, canonical_root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    match candidate_file(path)? {
        Some(canonical) => {
            ensure_inside_root(path, &canonical, canonical_root)?;
            Ok(Some(canonical))
        }
        None => Ok(None),
    }
}

fn candidate_file(path: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
    let Some(metadata) = optional_metadata(path)? else {
        return Ok(None);
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
    canonical_regular_file(path).map(Some)
}

fn ensure_inside_root(path: &Path, canonical: &Path, root: &Path) -> Result<(), ClangAdapterError> {
    if !canonical.starts_with(root) {
        return Err(invalid_data_error(
            path,
            format!(
                "automatically discovered database resolves outside project root {}",
                root.display()
            ),
        ));
    }
    Ok(())
}

fn optional_metadata(path: &Path) -> Result<Option<fs::Metadata>, ClangAdapterError> {
    fs::symlink_metadata(path).map(Some).or_else(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(read_error(path, source))
        }
    })
}

fn invalid_data_error(path: &Path, message: impl Into<String>) -> ClangAdapterError {
    ClangAdapterError::Read {
        operation: "validate",
        path: display_path(path),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()),
    }
}

fn invalid_root(path: &Path) -> ClangAdapterError {
    ClangAdapterError::InvalidRoot {
        path: display_path(path),
    }
}

fn read_error(path: &Path, source: std::io::Error) -> ClangAdapterError {
    ClangAdapterError::Read {
        operation: "read",
        path: display_path(path),
        source,
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
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
    canonical_regular_file(path).and_then(load_canonical_database)
}

fn load_canonical_database(path: PathBuf) -> Result<CompilationDatabase, ClangAdapterError> {
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
    let (file, length) = open_database(path)?;
    let (initial_capacity, maximum_capacity) = database_capacities(path, length)?;
    read_bounded_database(file, path, initial_capacity, maximum_capacity)
}

fn open_database(path: &Path) -> Result<(File, u64), ClangAdapterError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(source) => return Err(read_error(path, source)),
    };
    let metadata = file.metadata().map_err(|source| read_error(path, source))?;
    validate_database_size(path, &metadata)?;
    Ok((file, metadata.len()))
}

fn read_bounded_database(
    file: File,
    path: &Path,
    initial_capacity: usize,
    maximum_capacity: usize,
) -> Result<Vec<u8>, ClangAdapterError> {
    let mut contents = Vec::with_capacity(initial_capacity.saturating_add(1));
    file.take(MAX_DATABASE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|source| read_error(path, source))?;
    enforce_database_limit(path, contents.len(), maximum_capacity, " while reading")?;
    Ok(contents)
}

fn validate_database_size(path: &Path, metadata: &fs::Metadata) -> Result<(), ClangAdapterError> {
    ensure_regular(path, metadata)?;
    let actual = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let maximum = usize::try_from(MAX_DATABASE_BYTES).unwrap_or(usize::MAX);
    enforce_database_limit(path, actual, maximum, "")
}

fn enforce_database_limit(
    path: &Path,
    actual: usize,
    maximum: usize,
    context: &str,
) -> Result<(), ClangAdapterError> {
    if actual <= maximum {
        return Ok(());
    }
    Err(invalid_data_error(
        path,
        format!("database exceeds the {MAX_DATABASE_BYTES}-byte limit{context}"),
    ))
}

fn database_capacities(path: &Path, length: u64) -> Result<(usize, usize), ClangAdapterError> {
    let initial = usize::try_from(length)
        .map_err(|_| invalid_data_error(path, "database size cannot be represented on this platform"))?;
    let maximum = usize::try_from(MAX_DATABASE_BYTES)
        .map_err(|_| invalid_data_error(path, "database limit cannot be represented on this platform"))?;
    Ok((initial, maximum))
}

fn compile_command_from_raw(
    path: &Path,
    database_directory: &Path,
    index: usize,
    entry: RawCompileCommand,
) -> Result<CompileCommand, ClangAdapterError> {
    validate_entry_fields(path, index, &entry)?;
    let (arguments, origin) = command_arguments(path, index, entry.arguments, entry.command)?;
    let directory = resolve_path(database_directory, Path::new(&entry.directory));
    let file = resolve_path(&directory, Path::new(&entry.file));
    Ok(CompileCommand {
        directory,
        file,
        arguments,
        output: entry.output,
        origin,
    })
}

fn command_arguments(
    path: &Path,
    index: usize,
    arguments: Option<LimitedArguments>,
    command: Option<String>,
) -> Result<(Vec<String>, CommandOrigin), ClangAdapterError> {
    match (arguments, command) {
        (Some(arguments), None) => arguments_origin(path, index, arguments.0),
        (None, Some(command)) => command_origin(path, index, command),
        (Some(_), Some(_)) => Err(invalid_entry(
            path,
            index,
            "both arguments and command are present",
        )),
        (None, None) => Err(invalid_entry(
            path,
            index,
            "neither arguments nor command is present",
        )),
    }
}

fn arguments_origin(
    path: &Path,
    index: usize,
    arguments: Vec<String>,
) -> Result<(Vec<String>, CommandOrigin), ClangAdapterError> {
    if arguments.is_empty() {
        return Err(invalid_entry(path, index, "arguments is empty"));
    }
    validate_arguments(path, index, &arguments)?;
    Ok((arguments.clone(), CommandOrigin::Arguments(arguments)))
}

fn command_origin(
    path: &Path,
    index: usize,
    command: String,
) -> Result<(Vec<String>, CommandOrigin), ClangAdapterError> {
    validate_limit(path, index, command.len(), MAX_COMMAND_BYTES, EntryLimit::Command)?;
    let arguments = tokenize_command(&command).map_err(|source| ClangAdapterError::Tokenize {
        path: path.display().to_string(),
        index,
        source,
    })?;
    validate_arguments(path, index, &arguments)?;
    Ok((arguments, CommandOrigin::Command(command)))
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&base.join(path))
    }
}

fn validate_entry_fields(
    path: &Path,
    index: usize,
    entry: &RawCompileCommand,
) -> Result<(), ClangAdapterError> {
    if entry.directory.is_empty() || entry.file.is_empty() {
        return Err(invalid_entry(path, index, "directory and file must be non-empty"));
    }
    for (name, value) in [
        ("directory", entry.directory.as_str()),
        ("file", entry.file.as_str()),
    ] {
        validate_limit(
            path,
            index,
            value.len(),
            MAX_PATH_FIELD_BYTES,
            EntryLimit::Path(name),
        )?;
    }
    let output_length = entry.output.as_ref().map_or(0, String::len);
    validate_limit(
        path,
        index,
        output_length,
        MAX_PATH_FIELD_BYTES,
        EntryLimit::Path("output"),
    )?;
    Ok(())
}

fn validate_arguments(path: &Path, index: usize, arguments: &[String]) -> Result<(), ClangAdapterError> {
    validate_limit(
        path,
        index,
        arguments.len(),
        MAX_ARGUMENTS_PER_ENTRY,
        EntryLimit::ArgumentCount,
    )?;
    let mut total_bytes = 0_usize;
    for argument in arguments {
        validate_limit(path, index, argument.len(), MAX_ARGUMENT_BYTES, EntryLimit::Token)?;
        total_bytes = total_bytes.saturating_add(argument.len());
        validate_limit(
            path,
            index,
            total_bytes,
            MAX_ARGUMENT_BYTES_PER_ENTRY,
            EntryLimit::Aggregate,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EntryLimit<'a> {
    Command,
    Path(&'a str),
    ArgumentCount,
    Token,
    Aggregate,
}

impl EntryLimit<'_> {
    fn message(&self, maximum: usize) -> String {
        match self {
            Self::Command => format!("command exceeds the {maximum}-byte limit"),
            Self::Path(name) => format!("{name} exceeds the {maximum}-byte limit"),
            Self::ArgumentCount => format!("argv exceeds the {maximum}-argument limit"),
            Self::Token => format!("argument exceeds the {maximum}-byte token limit"),
            Self::Aggregate => format!("argv exceeds the {maximum}-byte aggregate limit"),
        }
    }
}

fn validate_limit(
    path: &Path,
    index: usize,
    actual: usize,
    maximum: usize,
    kind: EntryLimit<'_>,
) -> Result<(), ClangAdapterError> {
    if actual > maximum {
        Err(invalid_entry(path, index, kind.message(maximum)))
    } else {
        Ok(())
    }
}

fn invalid_entry(path: &Path, index: usize, message: impl Into<String>) -> ClangAdapterError {
    ClangAdapterError::InvalidEntry {
        path: display_path(path),
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
    LANGUAGE_EXTENSIONS
        .split(';')
        .find_map(|mapping| language_mapping(mapping, extension))
}

fn language_mapping(mapping: &str, extension: &str) -> Option<ClangLanguage> {
    let (extensions, language) = mapping.split_once('=')?;
    extensions
        .split(',')
        .any(|candidate| candidate == extension)
        .then(|| parse_language_name(language))
        .flatten()
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

    use super::*;
    use crate::test_support::{compile_command, create_dir, expect_error, temp_dir, write, write_database};

    fn project_command(file: &str, arguments: &[&str]) -> CompileCommand {
        compile_command(Path::new("/project"), file, arguments)
    }

    fn assert_adapter_error<T: std::fmt::Debug>(result: Result<T, ClangAdapterError>, expected: &str) {
        assert!(expect_error(result).to_string().contains(expected));
    }

    fn discover(root: &Path) -> Option<PathBuf> {
        discover_compilation_database(root).unwrap_or_else(|error| panic!("discover database: {error}"))
    }

    fn discovered(root: &Path) -> PathBuf {
        discover(root).unwrap_or_else(|| panic!("missing compilation database"))
    }

    fn database_fixture(contents: impl AsRef<[u8]>) -> (tempfile::TempDir, PathBuf) {
        let temp = temp_dir();
        let database = write_database(temp.path(), contents);
        (temp, database)
    }

    fn assert_database_rejected(contents: impl AsRef<[u8]>, expected: &str) {
        let (_temp, database) = database_fixture(contents);
        assert_adapter_error(load_database(&database), expected);
    }

    fn entry_database(field: &str, value: impl serde::Serialize) -> String {
        let mut entry = serde_json::json!({"directory": ".", "file": "a.c"});
        entry[field] = serde_json::to_value(value)
            .unwrap_or_else(|error| panic!("serialize database fixture field: {error}"));
        serde_json::json!([entry]).to_string()
    }

    fn raw_entry(file: String, output: Option<String>) -> RawCompileCommand {
        RawCompileCommand {
            directory: ".".to_string(),
            file,
            arguments: None,
            command: Some("clang a.c".to_string()),
            output,
        }
    }

    #[cfg(unix)]
    fn database_symlink(project: &Path) -> PathBuf {
        let target = project.join("actual-database.json");
        write(&target, "[]");
        let candidate = project.join(DATABASE_NAME);
        std::os::unix::fs::symlink(target, &candidate)
            .unwrap_or_else(|error| panic!("link database fixture: {error}"));
        candidate
    }

    #[cfg(unix)]
    fn outside_build_symlink(project: &Path) -> tempfile::TempDir {
        let external = temp_dir();
        let build = external.path().join("external-build");
        create_dir(&build);
        write_database(&build, "[]");
        std::os::unix::fs::symlink(build, project.join("build"))
            .unwrap_or_else(|error| panic!("link external build fixture: {error}"));
        external
    }

    #[test]
    fn discovers_root_before_build_and_never_generates_a_database() {
        let temp = temp_dir();
        assert_eq!(discover(temp.path()), None);
        assert!(!temp.path().join(DATABASE_NAME).exists());

        create_dir(temp.path().join("build"));
        write(temp.path().join("build").join(DATABASE_NAME), "[]");
        let build = discovered(temp.path());
        assert!(build.ends_with("build/compile_commands.json"));

        write(temp.path().join(DATABASE_NAME), "[]");
        let root = discovered(temp.path());
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
        const DATABASE: &str = r#"[
                {"directory":".","file":"src/a.c","arguments":["clang","-c","src/a.c"]},
                {"directory":".","file":"src/b.cpp","command":"clang++ -c 'src/b.cpp'","output":"b.o"}
            ]"#;
        let (temp, database) = database_fixture(DATABASE);

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
        let temp = temp_dir();
        let database = temp.path().join(DATABASE_NAME);
        let file = match File::create(&database) {
            Ok(file) => file,
            Err(error) => panic!("create sparse db: {error}"),
        };
        file.set_len(MAX_DATABASE_BYTES + 1)
            .unwrap_or_else(|error| panic!("size sparse db: {error}"));

        assert_adapter_error(load_database(&database), "67108864-byte limit");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_regular_socket_without_opening_it_as_a_database() {
        use std::os::unix::net::UnixListener;

        let temp = temp_dir();
        let socket = temp.path().join(DATABASE_NAME);
        let _listener = UnixListener::bind(&socket).unwrap_or_else(|error| panic!("bind socket: {error}"));

        assert_adapter_error(load_database(&socket), "expected a regular file");
    }

    #[test]
    fn rejects_non_regular_database_paths() {
        let temp = temp_dir();
        assert_adapter_error(load_database(temp.path()), "expected a regular file");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_database_symlinks_and_out_of_root_intermediate_symlinks() {
        let project = temp_dir();
        let candidate = database_symlink(project.path());

        assert_adapter_error(
            discover_compilation_database(project.path()),
            "symbolic links are not accepted",
        );
        assert_adapter_error(load_database(&candidate), "symbolic links are not accepted");

        fs::remove_file(&candidate).unwrap_or_else(|error| panic!("remove in-root symlink: {error}"));
        let _external = outside_build_symlink(project.path());

        assert_adapter_error(
            discover_compilation_database(project.path()),
            "resolves outside project root",
        );
    }

    #[test]
    fn bounds_immediate_child_enumeration() {
        let temp = temp_dir();
        for index in 0..=MAX_DISCOVERY_ENTRIES {
            File::create(temp.path().join(format!("entry-{index}")))
                .unwrap_or_else(|error| panic!("create discovery entry {index}: {error}"));
        }

        assert_adapter_error(
            discover_compilation_database(temp.path()),
            "4096-entry discovery limit",
        );
    }

    #[test]
    fn rejects_database_entry_count_above_limit_while_deserializing() {
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
        assert_database_rejected(contents, "exceeds the 100000-entry limit");
    }

    #[test]
    fn bounds_command_and_argument_payloads() {
        let command = "x".repeat(MAX_COMMAND_BYTES + 1);
        assert_database_rejected(
            entry_database("command", command),
            "command exceeds the 1048576-byte limit",
        );

        let oversized_token = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        assert_database_rejected(
            entry_database("arguments", ["clang".to_string(), oversized_token]),
            "65536-byte token limit",
        );

        let aggregate_arguments = vec!["x".repeat(MAX_ARGUMENT_BYTES); 17];
        assert_database_rejected(
            entry_database("arguments", aggregate_arguments),
            "1048576-byte aggregate limit",
        );

        let arguments = vec!["x"; MAX_ARGUMENTS_PER_ENTRY + 1];
        assert_database_rejected(entry_database("arguments", arguments), "4096-argument limit");

        let oversized_path = "x".repeat(MAX_PATH_FIELD_BYTES + 1);
        assert_database_rejected(
            entry_database("directory", oversized_path),
            "directory exceeds the 65536-byte limit",
        );
    }

    #[test]
    fn direct_entry_validation_enforces_every_bound() {
        let path = Path::new("compile_commands.json");
        let valid = raw_entry("a.c".to_string(), Some("a.o".to_string()));
        assert!(validate_entry_fields(path, 0, &valid).is_ok());

        let mut empty = valid;
        empty.file.clear();
        assert_adapter_error(validate_entry_fields(path, 0, &empty), "must be non-empty");

        let oversized_file = raw_entry("x".repeat(MAX_PATH_FIELD_BYTES + 1), None);
        assert!(validate_entry_fields(path, 0, &oversized_file).is_err());
        let oversized_output = raw_entry("a.c".to_string(), Some("x".repeat(MAX_PATH_FIELD_BYTES + 1)));
        assert!(validate_entry_fields(path, 0, &oversized_output).is_err());

        assert!(validate_arguments(path, 0, &["clang".to_string()]).is_ok());
        assert!(validate_arguments(path, 0, &vec!["x".to_string(); MAX_ARGUMENTS_PER_ENTRY + 1]).is_err());
        assert!(validate_arguments(path, 0, &["x".repeat(MAX_ARGUMENT_BYTES + 1)]).is_err());
        assert!(validate_arguments(path, 0, &vec!["x".repeat(MAX_ARGUMENT_BYTES); 17]).is_err());
    }

    #[test]
    fn path_normalization_and_explicit_language_cover_boundary_forms() {
        const CASES: &str = "clang a.c|none
clang -xc++ a.c|cpp
clang -x none a.c|none
clang -x|unsupported
clang -xcuda a.c|unsupported";

        assert_eq!(normalize_path(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize_path(Path::new("../a")), PathBuf::from("../a"));

        for case in CASES.lines() {
            let (arguments, expected) = case
                .split_once('|')
                .unwrap_or_else(|| panic!("invalid explicit-language case: {case}"));
            let arguments = crate::test_support::owned_words(arguments);
            assert_eq!(
                explicit_language(&arguments),
                expected_explicit_language(expected)
            );
        }
    }

    #[test]
    fn rejects_ambiguous_or_shell_dependent_commands() {
        let (_temp, database) =
            database_fixture(r#"[{"directory":".","file":"a.c","command":"clang a.c && touch bad"}]"#);
        assert!(matches!(
            load_database(&database),
            Err(ClangAdapterError::Tokenize { .. })
        ));
    }

    #[test]
    fn classifies_all_clang_language_modes() {
        const CASES: &str = "a.c|clang a.c|c
a.C|clang a.C|cpp
a.m|clang a.m|objective-c
a.mm|clang++ a.mm|objective-cpp
a.c|clang -x objective-c++ a.c|objective-cpp
        header.h|clang++ header.h|cpp
a.c|clang -x cuda a.c|none";
        for case in CASES.lines() {
            let fields = case.split('|').collect::<Vec<_>>();
            let [file, arguments, expected] = fields.as_slice() else {
                panic!("invalid language case: {case}");
            };
            let arguments = arguments.split_ascii_whitespace().collect::<Vec<_>>();
            assert_eq!(
                ClangLanguage::classify(&project_command(file, &arguments)),
                expected_language(expected)
            );
        }
    }

    fn expected_language(name: &str) -> Option<ClangLanguage> {
        match name {
            "c" => Some(ClangLanguage::C),
            "cpp" => Some(ClangLanguage::Cpp),
            "objective-c" => Some(ClangLanguage::ObjectiveC),
            "objective-cpp" => Some(ClangLanguage::ObjectiveCpp),
            _ => None,
        }
    }

    fn expected_explicit_language(name: &str) -> ExplicitLanguage {
        match name {
            "none" => ExplicitLanguage::NotSpecified,
            "unsupported" => ExplicitLanguage::Unsupported,
            other => {
                expected_language(other).map_or(ExplicitLanguage::Unsupported, ExplicitLanguage::Supported)
            }
        }
    }
}
