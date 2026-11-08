//! Bounded trusted configuration and deterministic language-server discovery.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::process::ProcessSpec;

/// Environment variable whose value overrides the default user configuration path.
pub const CONFIGURATION_PATH_ENVIRONMENT_VARIABLE: &str = "SRCMV_CONFIG";
/// Maximum accepted user-configuration file size.
pub const MAX_CONFIGURATION_BYTES: usize = 1024 * 1024;
/// Maximum nesting depth accepted in configuration and JSON-valued fields.
pub const MAX_CONFIGURATION_DEPTH: usize = 32;
/// Maximum number of literal server arguments.
pub const MAX_SERVER_ARGUMENTS: usize = 128;
/// Maximum UTF-8 size of one literal server argument.
pub const MAX_SERVER_ARGUMENT_BYTES: usize = 16 * 1024;
/// Maximum cumulative UTF-8 size of literal server arguments.
pub const MAX_TOTAL_SERVER_ARGUMENT_BYTES: usize = 256 * 1024;
/// Maximum serialized size of initialization options or settings.
pub const MAX_CONFIGURATION_JSON_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 size of a normalized descriptor ID.
pub const MAX_SERVER_ID_BYTES: usize = 255;
/// Maximum UTF-8 size of an extension or language ID.
pub const MAX_LANGUAGE_IDENTIFIER_BYTES: usize = 255;
/// Maximum byte size of one configured executable value.
pub const MAX_SERVER_PROGRAM_BYTES: usize = 16 * 1024;

const CONFIGURATION_VERSION: u32 = 1;
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 300_000;

/// Returns a configuration path without reading or mutating process-global environment.
///
/// An explicitly supplied override is returned exactly. Otherwise the path is
/// `srcmv/config.toml` below the supplied platform configuration directory.
#[must_use]
pub fn configuration_path(
    explicit_override: Option<&OsStr>,
    platform_configuration_directory: Option<&Path>,
) -> Option<PathBuf> {
    explicit_override.map(PathBuf::from).or_else(|| {
        platform_configuration_directory
            .map(|directory| directory.join("srcmv").join("config.toml"))
    })
}

/// Returns the current platform's default configuration path unless overridden.
///
/// The caller is responsible for reading `SRCMV_CONFIG` and passing its
/// value here. This function performs no environment mutation and creates no
/// directory or file.
#[must_use]
pub fn user_configuration_path(explicit_override: Option<&OsStr>) -> Option<PathBuf> {
    let base = directories::BaseDirs::new();
    configuration_path(
        explicit_override,
        base.as_ref().map(directories::BaseDirs::config_dir),
    )
}

/// A validated trusted user configuration.
pub struct UserConfiguration {
    servers: Vec<ServerDescriptor>,
}

impl UserConfiguration {
    /// Returns the validated server descriptors in document order.
    #[must_use]
    pub fn servers(&self) -> &[ServerDescriptor] {
        &self.servers
    }
}

impl fmt::Debug for UserConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserConfiguration")
            .field("server_count", &self.servers.len())
            .finish()
    }
}

/// One validated trusted language-server descriptor.
pub struct ServerDescriptor {
    id: String,
    extensions: Vec<String>,
    language_id: String,
    program: String,
    arguments: Vec<String>,
    initialization_options: Option<JsonValue>,
    settings: Option<JsonValue>,
    project_root: PathBuf,
    allow_workspace_program: bool,
    startup_timeout: Duration,
    request_timeout: Duration,
}

impl ServerDescriptor {
    /// Returns the normalized configuration ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the normalized extensions recognized by this descriptor.
    #[must_use]
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// Returns the LSP language ID.
    #[must_use]
    pub fn language_id(&self) -> &str {
        &self.language_id
    }
}

impl fmt::Debug for ServerDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerDescriptor")
            .field("id", &self.id)
            .field("extension_count", &self.extensions.len())
            .field("language_id_bytes", &self.language_id.len())
            .field("argument_count", &self.arguments.len())
            .field(
                "has_initialization_options",
                &self.initialization_options.is_some(),
            )
            .field("has_settings", &self.settings.is_some())
            .field("allow_workspace_program", &self.allow_workspace_program)
            .field("startup_timeout", &self.startup_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

/// Where a resolved server descriptor originated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerOrigin {
    /// An explicit program supplied by the CLI caller.
    Explicit,
    /// A trusted user-level configuration descriptor.
    UserConfiguration,
    /// srcmv's built-in convenience table.
    BuiltIn,
}

/// Explicit or automatic server choice for one resolution.
#[derive(Clone, Copy)]
pub enum ServerSelection<'a> {
    /// Select by extension, preferring trusted user configuration to built-ins.
    Automatic,
    /// Select a trusted user or built-in descriptor by normalized ID.
    Id(&'a str),
    /// Use an explicit program and literal arguments without PATH discovery.
    Program {
        /// Executable name or path passed directly to `std::process::Command`.
        program: &'a OsStr,
        /// Literal arguments; no shell parsing or interpolation is performed.
        arguments: &'a [String],
        /// Required LSP language ID.
        language_id: &'a str,
    },
}

impl fmt::Debug for ServerSelection<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automatic => formatter.write_str("Automatic"),
            Self::Id(id) => formatter
                .debug_struct("Id")
                .field("id_bytes", &id.len())
                .finish(),
            Self::Program {
                arguments,
                language_id,
                ..
            } => formatter
                .debug_struct("Program")
                .field("program", &"<redacted>")
                .field("argument_count", &arguments.len())
                .field("language_id_bytes", &language_id.len())
                .finish(),
        }
    }
}

/// Inputs used to resolve a language-server process.
#[derive(Clone, Copy)]
pub struct ResolutionRequest<'a> {
    /// Canonical srcmv workspace root.
    pub workspace_root: &'a Path,
    /// Source extension, with or without a leading dot.
    pub source_extension: &'a str,
    /// Explicit selection or automatic discovery.
    pub selection: ServerSelection<'a>,
    /// `PATH` value to search. The caller supplies it without environment mutation.
    pub executable_path: Option<&'a OsStr>,
}

impl fmt::Debug for ResolutionRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolutionRequest")
            .field("has_workspace_root", &true)
            .field("source_extension_bytes", &self.source_extension.len())
            .field("selection", &self.selection)
            .field("has_executable_path", &self.executable_path.is_some())
            .finish()
    }
}

/// A selected server ready to launch.
pub struct ResolvedServer {
    /// Selected descriptor ID, absent for an explicit program.
    pub configuration_id: Option<String>,
    /// LSP language ID sent with `didOpen`.
    pub language_id: String,
    /// Direct process specification. No shell is involved.
    pub process: ProcessSpec,
    /// Initialization options sent during initialization.
    pub initialization_options: Option<JsonValue>,
    /// Workspace settings sent after initialization and served to configuration requests.
    pub settings: Option<JsonValue>,
    /// Canonical working directory constrained to the workspace.
    pub project_root: PathBuf,
    /// Initialize/startup deadline.
    pub startup_timeout: Duration,
    /// Document-symbol request deadline.
    pub request_timeout: Duration,
    /// Trust/discovery source of this selection.
    pub origin: ServerOrigin,
}

impl fmt::Debug for ResolvedServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedServer")
            .field("configuration_id", &self.configuration_id)
            .field("language_id_bytes", &self.language_id.len())
            .field("process", &self.process)
            .field(
                "has_initialization_options",
                &self.initialization_options.is_some(),
            )
            .field("has_settings", &self.settings.is_some())
            .field("has_project_root", &true)
            .field("startup_timeout", &self.startup_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("origin", &self.origin)
            .finish()
    }
}

/// Bounded configuration or deterministic discovery failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// The configuration could not be opened or read.
    Read {
        /// Stable I/O error category; paths and OS messages are omitted.
        kind: io::ErrorKind,
    },
    /// The complete configuration exceeds its byte limit.
    ConfigurationTooLarge {
        /// Configured maximum bytes.
        limit: usize,
    },
    /// Configuration bytes are not valid UTF-8.
    NonUtf8Configuration,
    /// The bounded input is not valid TOML for the versioned schema.
    MalformedConfiguration,
    /// The configuration nesting exceeds its limit.
    ConfigurationTooDeep {
        /// Configured maximum nesting depth.
        limit: usize,
    },
    /// The configuration version is unsupported.
    UnsupportedVersion {
        /// Received version number.
        version: u32,
    },
    /// A descriptor has an empty ID, language ID, program, or extension.
    EmptyField,
    /// A server ID contains characters outside the stable identifier alphabet.
    InvalidServerId,
    /// A required descriptor field exceeds its dedicated byte limit.
    FieldTooLarge {
        /// Stable field name; the rejected content is omitted.
        field: &'static str,
        /// Maximum accepted bytes.
        limit: usize,
    },
    /// Two user descriptors have the same normalized ID.
    DuplicateServerId {
        /// Normalized duplicate ID.
        id: String,
    },
    /// One descriptor repeats an extension after normalization.
    DuplicateDescriptorExtension {
        /// Descriptor ID.
        id: String,
    },
    /// A workspace-relative project root is invalid or leaves the workspace.
    InvalidProjectRoot {
        /// Descriptor ID.
        id: String,
    },
    /// A configured timeout is outside the supported interval.
    InvalidTimeout {
        /// Descriptor ID.
        id: String,
    },
    /// Server arguments exceed one of the frozen bounds.
    InvalidArguments,
    /// Initialization options or settings exceed their depth or serialized-byte bound.
    OversizedJsonConfiguration {
        /// Descriptor ID.
        id: String,
    },
    /// More than one trusted descriptor recognizes the extension.
    AmbiguousExtension,
    /// No configured or built-in descriptor recognizes the requested selection.
    ServerNotConfigured,
    /// A requested descriptor ID does not exist.
    UnknownServerId {
        /// Normalized requested ID.
        id: String,
    },
    /// The selected executable could not be safely resolved.
    ExecutableNotFound {
        /// Selected descriptor ID.
        id: String,
    },
    /// Explicit program selection omitted its required language ID or program.
    InvalidExplicitSelection,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { kind } => write!(
                formatter,
                "could not read the user language-server configuration ({kind:?})"
            ),
            Self::ConfigurationTooLarge { limit } => write!(
                formatter,
                "user language-server configuration exceeds {limit} bytes"
            ),
            Self::NonUtf8Configuration => {
                formatter.write_str("user language-server configuration is not UTF-8")
            }
            Self::MalformedConfiguration => {
                formatter.write_str("user language-server configuration is malformed")
            }
            Self::ConfigurationTooDeep { limit } => write!(
                formatter,
                "user language-server configuration exceeds nesting depth {limit}"
            ),
            Self::UnsupportedVersion { version } => write!(
                formatter,
                "unsupported user language-server configuration version {version}"
            ),
            Self::EmptyField => {
                formatter.write_str("server descriptor contains an empty required field")
            }
            Self::InvalidServerId => formatter.write_str(
                "server configuration ID may contain only ASCII letters, digits, `.`, `_`, and `-`",
            ),
            Self::FieldTooLarge { field, limit } => {
                write!(formatter, "server descriptor {field} exceeds {limit} bytes")
            }
            Self::DuplicateServerId { id } => {
                write!(
                    formatter,
                    "duplicate normalized server configuration ID `{id}`"
                )
            }
            Self::DuplicateDescriptorExtension { id } => write!(
                formatter,
                "server configuration `{id}` repeats a normalized extension"
            ),
            Self::InvalidProjectRoot { id } => {
                write!(
                    formatter,
                    "server configuration `{id}` has an invalid project root"
                )
            }
            Self::InvalidTimeout { id } => write!(
                formatter,
                "server configuration `{id}` has a timeout outside supported bounds"
            ),
            Self::InvalidArguments => {
                formatter.write_str("server arguments exceed the configured resource limits")
            }
            Self::OversizedJsonConfiguration { id } => write!(
                formatter,
                "server configuration `{id}` contains oversized options or settings"
            ),
            Self::AmbiguousExtension => formatter.write_str(
                "multiple trusted servers recognize the extension; select a server ID explicitly",
            ),
            Self::ServerNotConfigured => {
                formatter.write_str("no language server is configured for the requested source")
            }
            Self::UnknownServerId { id } => {
                write!(
                    formatter,
                    "language-server configuration ID `{id}` was not found"
                )
            }
            Self::ExecutableNotFound { id } => write!(
                formatter,
                "language-server executable for configuration `{id}` was not found in a trusted location"
            ),
            Self::InvalidExplicitSelection => formatter
                .write_str("explicit language-server selection requires a program and language ID"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Loads and validates a trusted user configuration without creating filesystem entries.
///
/// # Errors
///
/// Returns [`ConfigError`] for I/O failure, resource-limit violation, malformed
/// TOML, unsupported versions, or invalid descriptors.
pub fn load_user_configuration(path: &Path) -> Result<UserConfiguration, ConfigError> {
    let mut file = File::open(path).map_err(read_error)?;
    let read_limit = u64::try_from(MAX_CONFIGURATION_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(MAX_CONFIGURATION_BYTES.min(64 * 1024));
    file.by_ref()
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() > MAX_CONFIGURATION_BYTES {
        return Err(ConfigError::ConfigurationTooLarge {
            limit: MAX_CONFIGURATION_BYTES,
        });
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::NonUtf8Configuration)?;
    parse_user_configuration(text)
}

/// Parses and validates a bounded user configuration already held in memory.
///
/// # Errors
///
/// Returns [`ConfigError`] when the UTF-8 byte limit, TOML/schema, depth, or
/// descriptor validation rules are violated.
pub fn parse_user_configuration(text: &str) -> Result<UserConfiguration, ConfigError> {
    if text.len() > MAX_CONFIGURATION_BYTES {
        return Err(ConfigError::ConfigurationTooLarge {
            limit: MAX_CONFIGURATION_BYTES,
        });
    }
    let value: toml::Value =
        toml::from_str(text).map_err(|_| ConfigError::MalformedConfiguration)?;
    validate_toml_depth(&value)?;
    let raw: RawConfiguration = value
        .try_into()
        .map_err(|_| ConfigError::MalformedConfiguration)?;
    validate_configuration(raw)
}

/// Resolves an explicit, trusted-user, or built-in descriptor into a process specification.
///
/// Resolution order is explicit selection, trusted user configuration, then
/// built-in descriptors. Automatic executable discovery searches only absolute
/// `PATH` entries and rejects workspace-local targets unless a trusted user
/// descriptor explicitly opts in.
///
/// # Errors
///
/// Returns [`ConfigError`] when selection is ambiguous, configuration is absent,
/// roots are invalid, arguments exceed bounds, or no safe executable is found.
pub fn resolve_server(
    configuration: Option<&UserConfiguration>,
    request: ResolutionRequest<'_>,
) -> Result<ResolvedServer, ConfigError> {
    let workspace_root =
        request
            .workspace_root
            .canonicalize()
            .map_err(|_| ConfigError::InvalidProjectRoot {
                id: "workspace".to_owned(),
            })?;

    match request.selection {
        ServerSelection::Program {
            program,
            arguments,
            language_id,
        } => resolve_explicit_program(&workspace_root, program, arguments, language_id),
        ServerSelection::Id(id) => {
            let id = normalize_id(id)?;
            if let Some(descriptor) = configuration.and_then(|configuration| {
                configuration.servers.iter().find(|server| server.id == id)
            }) {
                return resolve_user_descriptor(
                    descriptor,
                    &workspace_root,
                    request.executable_path,
                );
            }
            let builtin = BUILTIN_SERVERS
                .iter()
                .find(|descriptor| descriptor.id == id)
                .ok_or(ConfigError::UnknownServerId { id })?;
            resolve_builtin(builtin, None, &workspace_root, request.executable_path)
        }
        ServerSelection::Automatic => {
            let extension = normalize_extension(request.source_extension)?;
            if let Some(configuration) = configuration {
                let mut matches = configuration
                    .servers
                    .iter()
                    .filter(|server| server.extensions.iter().any(|item| item == &extension));
                if let Some(descriptor) = matches.next() {
                    if matches.next().is_some() {
                        return Err(ConfigError::AmbiguousExtension);
                    }
                    return resolve_user_descriptor(
                        descriptor,
                        &workspace_root,
                        request.executable_path,
                    );
                }
            }
            let mut matches = BUILTIN_SERVERS.iter().filter_map(|descriptor| {
                descriptor
                    .languages
                    .iter()
                    .find(|binding| binding.extension == extension)
                    .map(|binding| (descriptor, binding.language_id))
            });
            let Some((descriptor, language_id)) = matches.next() else {
                return Err(ConfigError::ServerNotConfigured);
            };
            if matches.next().is_some() {
                return Err(ConfigError::AmbiguousExtension);
            }
            resolve_builtin(
                descriptor,
                Some(language_id),
                &workspace_root,
                request.executable_path,
            )
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfiguration {
    version: u32,
    #[serde(default)]
    servers: Vec<RawServerDescriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerDescriptor {
    id: String,
    extensions: Vec<String>,
    language_id: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    initialization_options: Option<JsonValue>,
    #[serde(default)]
    settings: Option<JsonValue>,
    #[serde(default = "default_project_root")]
    project_root: PathBuf,
    #[serde(default)]
    allow_workspace_program: bool,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    request_timeout_ms: u64,
}

fn default_project_root() -> PathBuf {
    PathBuf::from(".")
}

const fn default_startup_timeout_ms() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

const fn default_request_timeout_ms() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_MS
}

fn read_error(error: io::Error) -> ConfigError {
    ConfigError::Read { kind: error.kind() }
}

fn validate_toml_depth(root: &toml::Value) -> Result<(), ConfigError> {
    let mut pending = vec![(root, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_CONFIGURATION_DEPTH {
            return Err(ConfigError::ConfigurationTooDeep {
                limit: MAX_CONFIGURATION_DEPTH,
            });
        }
        match value {
            toml::Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            toml::Value::Table(values) => {
                pending.extend(
                    values
                        .values()
                        .map(|value| (value, depth.saturating_add(1))),
                );
            }
            toml::Value::String(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }
    Ok(())
}

fn validate_json(value: &JsonValue, id: &str) -> Result<(), ConfigError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ConfigError::OversizedJsonConfiguration { id: id.to_owned() })?;
    if bytes.len() > MAX_CONFIGURATION_JSON_BYTES {
        return Err(ConfigError::OversizedJsonConfiguration { id: id.to_owned() });
    }
    let mut pending = vec![(value, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_CONFIGURATION_DEPTH {
            return Err(ConfigError::OversizedJsonConfiguration { id: id.to_owned() });
        }
        match value {
            JsonValue::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            JsonValue::Object(values) => {
                pending.extend(
                    values
                        .values()
                        .map(|value| (value, depth.saturating_add(1))),
                );
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
        }
    }
    Ok(())
}

fn validate_configuration(raw: RawConfiguration) -> Result<UserConfiguration, ConfigError> {
    if raw.version != CONFIGURATION_VERSION {
        return Err(ConfigError::UnsupportedVersion {
            version: raw.version,
        });
    }
    let mut ids = HashSet::new();
    let mut servers = Vec::with_capacity(raw.servers.len());
    for raw_server in raw.servers {
        let id = normalize_id(&raw_server.id)?;
        if !ids.insert(id.clone()) {
            return Err(ConfigError::DuplicateServerId { id });
        }
        let language_id = bounded_trimmed(
            &raw_server.language_id,
            "language ID",
            MAX_LANGUAGE_IDENTIFIER_BYTES,
        )?;
        let program = bounded_untrimmed(raw_server.program, "program", MAX_SERVER_PROGRAM_BYTES)?;
        validate_arguments(&raw_server.args)?;
        if let Some(initialization_options) = &raw_server.initialization_options {
            validate_json(initialization_options, &id)?;
        }
        if let Some(settings) = &raw_server.settings {
            validate_json(settings, &id)?;
        }
        let project_root = normalize_relative_project_root(&raw_server.project_root)
            .ok_or_else(|| ConfigError::InvalidProjectRoot { id: id.clone() })?;
        let startup_timeout = validate_timeout(raw_server.startup_timeout_ms, &id)?;
        let request_timeout = validate_timeout(raw_server.request_timeout_ms, &id)?;
        let mut seen_extensions = HashSet::new();
        let mut extensions = Vec::with_capacity(raw_server.extensions.len());
        if raw_server.extensions.is_empty() {
            return Err(ConfigError::EmptyField);
        }
        for extension in raw_server.extensions {
            let extension = normalize_extension(&extension)?;
            if !seen_extensions.insert(extension.clone()) {
                return Err(ConfigError::DuplicateDescriptorExtension { id });
            }
            extensions.push(extension);
        }
        servers.push(ServerDescriptor {
            id,
            extensions,
            language_id,
            program,
            arguments: raw_server.args,
            initialization_options: raw_server.initialization_options,
            settings: raw_server.settings,
            project_root,
            allow_workspace_program: raw_server.allow_workspace_program,
            startup_timeout,
            request_timeout,
        });
    }
    Ok(UserConfiguration { servers })
}

fn normalize_id(value: &str) -> Result<String, ConfigError> {
    let value = bounded_trimmed(value, "ID", MAX_SERVER_ID_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ConfigError::InvalidServerId);
    }
    Ok(value.to_ascii_lowercase())
}

fn normalize_extension(value: &str) -> Result<String, ConfigError> {
    let value = value.trim().strip_prefix('.').unwrap_or(value.trim());
    if value.is_empty() || value.contains('/') || value.contains('\\') {
        return Err(ConfigError::EmptyField);
    }
    if value.len() > MAX_LANGUAGE_IDENTIFIER_BYTES {
        return Err(ConfigError::FieldTooLarge {
            field: "extension",
            limit: MAX_LANGUAGE_IDENTIFIER_BYTES,
        });
    }
    Ok(value.to_ascii_lowercase())
}

fn bounded_trimmed(value: &str, field: &'static str, limit: usize) -> Result<String, ConfigError> {
    let value = value.trim();
    if value.is_empty() || value.contains('\0') {
        return Err(ConfigError::EmptyField);
    }
    if value.len() > limit {
        return Err(ConfigError::FieldTooLarge { field, limit });
    }
    Ok(value.to_owned())
}

fn bounded_untrimmed(
    value: String,
    field: &'static str,
    limit: usize,
) -> Result<String, ConfigError> {
    if value.is_empty() || value.contains('\0') {
        return Err(ConfigError::EmptyField);
    }
    if value.len() > limit {
        return Err(ConfigError::FieldTooLarge { field, limit });
    }
    Ok(value)
}

fn normalize_relative_project_root(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Some(normalized)
}

fn validate_timeout(milliseconds: u64, id: &str) -> Result<Duration, ConfigError> {
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&milliseconds) {
        return Err(ConfigError::InvalidTimeout { id: id.to_owned() });
    }
    Ok(Duration::from_millis(milliseconds))
}

fn validate_arguments(arguments: &[String]) -> Result<(), ConfigError> {
    if arguments.len() > MAX_SERVER_ARGUMENTS {
        return Err(ConfigError::InvalidArguments);
    }
    let mut total = 0_usize;
    for argument in arguments {
        let length = argument.len();
        if length > MAX_SERVER_ARGUMENT_BYTES || argument.contains('\0') {
            return Err(ConfigError::InvalidArguments);
        }
        total = total
            .checked_add(length)
            .ok_or(ConfigError::InvalidArguments)?;
        if total > MAX_TOTAL_SERVER_ARGUMENT_BYTES {
            return Err(ConfigError::InvalidArguments);
        }
    }
    Ok(())
}

fn resolve_explicit_program(
    workspace_root: &Path,
    program: &OsStr,
    arguments: &[String],
    language_id: &str,
) -> Result<ResolvedServer, ConfigError> {
    if program.is_empty() || program.len() > MAX_SERVER_PROGRAM_BYTES {
        return Err(ConfigError::InvalidExplicitSelection);
    }
    let language_id = bounded_trimmed(language_id, "language ID", MAX_LANGUAGE_IDENTIFIER_BYTES)?;
    validate_arguments(arguments)?;
    Ok(ResolvedServer {
        configuration_id: None,
        language_id,
        process: ProcessSpec::new(program)
            .args(arguments.iter().map(String::as_str))
            .current_directory(workspace_root),
        initialization_options: None,
        settings: None,
        project_root: workspace_root.to_owned(),
        startup_timeout: Duration::from_millis(DEFAULT_STARTUP_TIMEOUT_MS),
        request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
        origin: ServerOrigin::Explicit,
    })
}

fn resolve_user_descriptor(
    descriptor: &ServerDescriptor,
    workspace_root: &Path,
    executable_path: Option<&OsStr>,
) -> Result<ResolvedServer, ConfigError> {
    let project_root = workspace_root
        .join(&descriptor.project_root)
        .canonicalize()
        .map_err(|_| ConfigError::InvalidProjectRoot {
            id: descriptor.id.clone(),
        })?;
    if !project_root.is_dir() || !project_root.starts_with(workspace_root) {
        return Err(ConfigError::InvalidProjectRoot {
            id: descriptor.id.clone(),
        });
    }
    let executable = resolve_descriptor_executable(
        &descriptor.program,
        &project_root,
        workspace_root,
        executable_path,
        descriptor.allow_workspace_program,
    )
    .ok_or_else(|| ConfigError::ExecutableNotFound {
        id: descriptor.id.clone(),
    })?;
    Ok(ResolvedServer {
        configuration_id: Some(descriptor.id.clone()),
        language_id: descriptor.language_id.clone(),
        process: ProcessSpec::new(executable)
            .args(&descriptor.arguments)
            .current_directory(&project_root),
        initialization_options: descriptor.initialization_options.clone(),
        settings: descriptor.settings.clone(),
        project_root,
        startup_timeout: descriptor.startup_timeout,
        request_timeout: descriptor.request_timeout,
        origin: ServerOrigin::UserConfiguration,
    })
}

fn resolve_descriptor_executable(
    program: &str,
    project_root: &Path,
    workspace_root: &Path,
    executable_path: Option<&OsStr>,
    allow_workspace_program: bool,
) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.is_absolute() {
        return qualify_executable(program_path, workspace_root, allow_workspace_program);
    }
    if program_path.components().count() > 1 {
        if !allow_workspace_program {
            return None;
        }
        return qualify_executable(&project_root.join(program_path), workspace_root, true);
    }
    discover_executable(
        program_path.as_os_str(),
        executable_path,
        workspace_root,
        allow_workspace_program,
    )
}

fn discover_executable(
    program: &OsStr,
    executable_path: Option<&OsStr>,
    workspace_root: &Path,
    allow_workspace_program: bool,
) -> Option<PathBuf> {
    let executable_path = executable_path?;
    std::env::split_paths(executable_path)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            qualify_executable(
                &directory.join(program),
                workspace_root,
                allow_workspace_program,
            )
        })
}

fn qualify_executable(
    candidate: &Path,
    workspace_root: &Path,
    allow_workspace_program: bool,
) -> Option<PathBuf> {
    let canonical = candidate.canonicalize().ok()?;
    let metadata = canonical.metadata().ok()?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return None;
    }
    if canonical.starts_with(workspace_root) && !allow_workspace_program {
        return None;
    }
    Some(canonical)
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

struct BuiltinServer {
    id: &'static str,
    program: &'static str,
    arguments: &'static [&'static str],
    languages: &'static [BuiltinLanguage],
}

struct BuiltinLanguage {
    extension: &'static str,
    language_id: &'static str,
}

const BUILTIN_SERVERS: &[BuiltinServer] = &[
    BuiltinServer {
        id: "rust",
        program: "rust-analyzer",
        arguments: &[],
        languages: &[BuiltinLanguage {
            extension: "rs",
            language_id: "rust",
        }],
    },
    BuiltinServer {
        id: "go",
        program: "gopls",
        arguments: &[],
        languages: &[BuiltinLanguage {
            extension: "go",
            language_id: "go",
        }],
    },
    BuiltinServer {
        id: "python",
        program: "pylsp",
        arguments: &[],
        languages: &[BuiltinLanguage {
            extension: "py",
            language_id: "python",
        }],
    },
    BuiltinServer {
        id: "clangd-c",
        program: "clangd",
        arguments: &[],
        languages: &[BuiltinLanguage {
            extension: "c",
            language_id: "c",
        }],
    },
    BuiltinServer {
        id: "clangd-cpp",
        program: "clangd",
        arguments: &[],
        languages: &[
            BuiltinLanguage {
                extension: "cc",
                language_id: "cpp",
            },
            BuiltinLanguage {
                extension: "cpp",
                language_id: "cpp",
            },
            BuiltinLanguage {
                extension: "cxx",
                language_id: "cpp",
            },
        ],
    },
    BuiltinServer {
        id: "swift",
        program: "sourcekit-lsp",
        arguments: &[],
        languages: &[BuiltinLanguage {
            extension: "swift",
            language_id: "swift",
        }],
    },
    BuiltinServer {
        id: "typescript",
        program: "typescript-language-server",
        arguments: &["--stdio"],
        languages: &[
            BuiltinLanguage {
                extension: "ts",
                language_id: "typescript",
            },
            BuiltinLanguage {
                extension: "tsx",
                language_id: "typescriptreact",
            },
            BuiltinLanguage {
                extension: "js",
                language_id: "javascript",
            },
            BuiltinLanguage {
                extension: "jsx",
                language_id: "javascriptreact",
            },
        ],
    },
];

fn resolve_builtin(
    descriptor: &BuiltinServer,
    automatic_language_id: Option<&str>,
    workspace_root: &Path,
    executable_path: Option<&OsStr>,
) -> Result<ResolvedServer, ConfigError> {
    let executable = discover_executable(
        OsStr::new(descriptor.program),
        executable_path,
        workspace_root,
        false,
    )
    .ok_or_else(|| ConfigError::ExecutableNotFound {
        id: descriptor.id.to_owned(),
    })?;
    let language_id = automatic_language_id
        .or_else(|| {
            descriptor
                .languages
                .first()
                .map(|binding| binding.language_id)
        })
        .ok_or(ConfigError::ServerNotConfigured)?;
    Ok(ResolvedServer {
        configuration_id: Some(descriptor.id.to_owned()),
        language_id: language_id.to_owned(),
        process: ProcessSpec::new(executable)
            .args(descriptor.arguments.iter().copied())
            .current_directory(workspace_root),
        initialization_options: None,
        settings: None,
        project_root: workspace_root.to_owned(),
        startup_timeout: Duration::from_millis(DEFAULT_STARTUP_TIMEOUT_MS),
        request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
        origin: ServerOrigin::BuiltIn,
    })
}
