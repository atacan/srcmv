//! Integration tests for bounded trusted configuration and executable discovery.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use srcmv_lsp::config::{
    ConfigError, MAX_CONFIGURATION_BYTES, MAX_CONFIGURATION_JSON_BYTES,
    MAX_LANGUAGE_IDENTIFIER_BYTES, MAX_SERVER_ARGUMENT_BYTES, MAX_SERVER_ARGUMENTS,
    MAX_SERVER_ID_BYTES, MAX_SERVER_PROGRAM_BYTES, MAX_TOTAL_SERVER_ARGUMENT_BYTES,
    ResolutionRequest, ServerOrigin, ServerSelection, configuration_path, load_user_configuration,
    parse_user_configuration, resolve_server,
};

static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "srcmv-config-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn directory(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(&path).expect("create test subdirectory");
        path
    }

    fn executable(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create executable parent");
        }
        fs::write(&path, b"test executable").expect("write executable fixture");
        make_executable(&path);
        path.canonicalize()
            .expect("canonicalize executable fixture")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .expect("read executable metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("mark fixture executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn config_with_server(id: &str, extensions: &str, program: &str, extra: &str) -> String {
    format!(
        r#"version = 1

[[servers]]
id = "{id}"
extensions = [{extensions}]
language_id = "fixture"
program = "{program}"
{extra}
"#
    )
}

fn config_with_arguments(arguments: &[String]) -> String {
    let arguments = arguments
        .iter()
        .map(|argument| format!(r#""{argument}""#))
        .collect::<Vec<_>>()
        .join(",");
    config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        &format!("args = [{arguments}]"),
    )
}

fn configuration_with_exact_bytes(length: usize) -> String {
    let mut document = "version = 1\n".to_owned();
    assert!(length >= document.len());
    if length > document.len() {
        document.push('#');
        document.extend(std::iter::repeat_n('x', length - document.len()));
    }
    document.truncate(length);
    document
}

fn automatic_request<'a>(
    workspace: &'a Path,
    extension: &'a str,
    executable_path: Option<&'a OsStr>,
) -> ResolutionRequest<'a> {
    ResolutionRequest {
        workspace_root: workspace,
        source_extension: extension,
        selection: ServerSelection::Automatic,
        executable_path,
    }
}

#[test]
fn configuration_path_should_use_explicit_override_exactly() {
    let override_path = OsStr::new("relative/custom.toml");

    let result = configuration_path(Some(override_path), Some(Path::new("/ignored")));

    assert_eq!(result, Some(PathBuf::from("relative/custom.toml")));
}

#[test]
fn configuration_path_should_preserve_linux_layout() {
    let result = configuration_path(None, Some(Path::new("/home/alice/.config")));

    assert_eq!(
        result,
        Some(PathBuf::from("/home/alice/.config/srcmv/config.toml"))
    );
}

#[test]
fn configuration_path_should_preserve_macos_layout() {
    let result = configuration_path(
        None,
        Some(Path::new("/Users/alice/Library/Application Support")),
    );

    assert_eq!(
        result,
        Some(PathBuf::from(
            "/Users/alice/Library/Application Support/srcmv/config.toml"
        ))
    );
}

#[test]
fn loading_missing_configuration_should_not_create_files_or_directories() {
    let root = TestDirectory::new("missing");
    let path = root.path().join("absent/config.toml");

    let result = load_user_configuration(&path);

    assert!(matches!(result, Err(ConfigError::Read { .. })) && !path.parent().unwrap().exists());
}

#[test]
fn parser_should_validate_and_normalize_descriptor_identity() {
    let configuration = parse_user_configuration(&config_with_server(
        "  RUST-CUSTOM  ",
        "\".RS\"",
        "rust-analyzer",
        "",
    ))
    .expect("valid descriptor");

    assert_eq!(configuration.servers()[0].id(), "rust-custom");
    assert_eq!(configuration.servers()[0].extensions(), &["rs"]);
}

#[test]
fn parser_should_accept_id_at_response_schema_limit() {
    let id = "a".repeat(MAX_SERVER_ID_BYTES);

    let result = parse_user_configuration(&config_with_server(&id, "\"fixture\"", "server", ""));

    assert!(result.is_ok(), "at-limit ID should be valid: {result:?}");
}

#[test]
fn parser_should_reject_id_above_response_schema_limit() {
    let id = "a".repeat(MAX_SERVER_ID_BYTES + 1);

    let result = parse_user_configuration(&config_with_server(&id, "\"fixture\"", "server", ""));

    assert!(matches!(
        result,
        Err(ConfigError::FieldTooLarge {
            field: "ID",
            limit: MAX_SERVER_ID_BYTES
        })
    ));
}

#[test]
fn parser_should_reject_control_or_bidirectional_characters_in_server_id() {
    let id = r"safe\nunsafe\u202e";

    let result = parse_user_configuration(&config_with_server(id, "\"fixture\"", "server", ""));

    assert!(matches!(result, Err(ConfigError::InvalidServerId)));
}

#[test]
fn parser_should_accept_id_below_response_schema_limit() {
    let id = "a".repeat(MAX_SERVER_ID_BYTES - 1);

    let result = parse_user_configuration(&config_with_server(&id, "\"fixture\"", "server", ""));

    assert!(result.is_ok(), "below-limit ID should be valid: {result:?}");
}

#[test]
fn parser_should_reject_duplicate_ids_after_normalization() {
    let document = r#"version = 1
[[servers]]
id = "Rust"
extensions = ["rs"]
language_id = "rust"
program = "one"
[[servers]]
id = " rust "
extensions = ["rx"]
language_id = "rust"
program = "two"
"#;

    let result = parse_user_configuration(document);

    assert!(matches!(result, Err(ConfigError::DuplicateServerId { ref id }) if id == "rust"));
}

#[test]
fn parser_should_reject_repeated_extensions_within_a_descriptor() {
    let result = parse_user_configuration(&config_with_server(
        "fixture",
        "\"RS\", \".rs\"",
        "server",
        "",
    ));

    assert!(matches!(
        result,
        Err(ConfigError::DuplicateDescriptorExtension { ref id }) if id == "fixture"
    ));
}

#[test]
fn parser_should_reject_configuration_above_byte_limit() {
    let document = " ".repeat(MAX_CONFIGURATION_BYTES + 1);

    let result = parse_user_configuration(&document);

    assert!(matches!(
        result,
        Err(ConfigError::ConfigurationTooLarge {
            limit: MAX_CONFIGURATION_BYTES
        })
    ));
}

#[test]
fn parser_should_accept_configuration_below_and_at_byte_limit() {
    for length in [MAX_CONFIGURATION_BYTES - 1, MAX_CONFIGURATION_BYTES] {
        let document = configuration_with_exact_bytes(length);
        let result = parse_user_configuration(&document);

        assert!(
            result.is_ok(),
            "configuration of {length} bytes should be accepted: {result:?}"
        );
    }
}

#[test]
fn parser_should_reject_configuration_above_depth_limit() {
    let mut nested = "true".to_owned();
    for _ in 0..40 {
        nested = format!("{{ value = {nested} }}");
    }
    let document = format!("version = 1\nextra = {nested}\n");

    let result = parse_user_configuration(&document);

    assert!(matches!(
        result,
        Err(ConfigError::ConfigurationTooDeep { .. })
    ));
}

#[test]
fn parser_should_accept_configuration_at_depth_limit() {
    // Root table, servers array, and server table consume the first three
    // levels. Twenty-eight inline tables plus the boolean reach level 32.
    let mut settings = "true".to_owned();
    for _ in 0..28 {
        settings = format!("{{ value = {settings} }}");
    }
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        &format!("settings = {settings}"),
    );

    let result = parse_user_configuration(&document);

    assert!(
        result.is_ok(),
        "configuration at depth 32 should pass: {result:?}"
    );
}

#[test]
fn parser_should_enforce_language_and_extension_byte_limits() {
    let at_language = "l".repeat(MAX_LANGUAGE_IDENTIFIER_BYTES);
    let at_extension = "e".repeat(MAX_LANGUAGE_IDENTIFIER_BYTES);
    let at = config_with_server(
        "fixture",
        &format!("\"{at_extension}\""),
        "server",
        &format!("language_id = \"{at_language}\""),
    )
    .replace("language_id = \"fixture\"\n", "");
    assert!(parse_user_configuration(&at).is_ok());

    let above_language = "l".repeat(MAX_LANGUAGE_IDENTIFIER_BYTES + 1);
    let language_result = parse_user_configuration(
        &config_with_server(
            "fixture",
            "\"fixture\"",
            "server",
            &format!("language_id = \"{above_language}\""),
        )
        .replace("language_id = \"fixture\"\n", ""),
    );
    assert!(matches!(
        language_result,
        Err(ConfigError::FieldTooLarge {
            field: "language ID",
            limit: MAX_LANGUAGE_IDENTIFIER_BYTES
        })
    ));

    let above_extension = "e".repeat(MAX_LANGUAGE_IDENTIFIER_BYTES + 1);
    let extension_result = parse_user_configuration(&config_with_server(
        "fixture",
        &format!("\"{above_extension}\""),
        "server",
        "",
    ));
    assert!(matches!(
        extension_result,
        Err(ConfigError::FieldTooLarge {
            field: "extension",
            limit: MAX_LANGUAGE_IDENTIFIER_BYTES
        })
    ));
}

#[test]
fn parser_should_accept_program_at_limit_and_reject_above() {
    let at_program = "p".repeat(MAX_SERVER_PROGRAM_BYTES);
    let at = parse_user_configuration(&config_with_server(
        "fixture",
        "\"fixture\"",
        &at_program,
        "",
    ));
    assert!(at.is_ok(), "program at byte limit should pass: {at:?}");

    let above_program = "p".repeat(MAX_SERVER_PROGRAM_BYTES + 1);
    let above = parse_user_configuration(&config_with_server(
        "fixture",
        "\"fixture\"",
        &above_program,
        "",
    ));
    assert!(matches!(
        above,
        Err(ConfigError::FieldTooLarge {
            field: "program",
            limit: MAX_SERVER_PROGRAM_BYTES
        })
    ));
}

#[test]
fn parser_should_reject_json_field_whose_serialized_form_exceeds_limit() {
    let newlines = "\n".repeat(530_000);
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        &format!("settings = {{ payload = '''{newlines}''' }}"),
    );

    let result = parse_user_configuration(&document);

    assert!(matches!(
        result,
        Err(ConfigError::OversizedJsonConfiguration { ref id }) if id == "fixture"
    ));
}

#[test]
fn parser_should_accept_json_field_at_exact_serialized_limit() {
    let fixed_json_bytes = serde_json::to_vec(&serde_json::json!({"payload": "xx"}))
        .expect("fixture JSON should serialize")
        .len();
    let newline_count = (MAX_CONFIGURATION_JSON_BYTES - fixed_json_bytes) / 2;
    let payload = format!("xx{}", "\n".repeat(newline_count));
    assert_eq!(
        serde_json::to_vec(&serde_json::json!({"payload": payload}))
            .expect("fixture JSON should serialize")
            .len(),
        MAX_CONFIGURATION_JSON_BYTES
    );
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        &format!(
            "settings = {{ payload = '''xx{}''' }}",
            "\n".repeat(newline_count)
        ),
    );

    let result = parse_user_configuration(&document);

    assert!(
        result.is_ok(),
        "at-limit JSON should be accepted: {result:?}"
    );
}

#[test]
fn parser_should_reject_argument_above_individual_limit() {
    let argument = "a".repeat(MAX_SERVER_ARGUMENT_BYTES + 1);
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        &format!("args = [\"{argument}\"]"),
    );

    let result = parse_user_configuration(&document);

    assert!(matches!(result, Err(ConfigError::InvalidArguments)));
}

#[test]
fn parser_should_accept_arguments_below_and_at_individual_limit() {
    for length in [MAX_SERVER_ARGUMENT_BYTES - 1, MAX_SERVER_ARGUMENT_BYTES] {
        let result = parse_user_configuration(&config_with_arguments(&["a".repeat(length)]));

        assert!(
            result.is_ok(),
            "argument of {length} bytes should be accepted: {result:?}"
        );
    }
}

#[test]
fn parser_should_enforce_argument_count_below_at_and_above_limit() {
    for count in [MAX_SERVER_ARGUMENTS - 1, MAX_SERVER_ARGUMENTS] {
        let arguments = vec!["x".to_owned(); count];
        let result = parse_user_configuration(&config_with_arguments(&arguments));

        assert!(
            result.is_ok(),
            "{count} arguments should be accepted: {result:?}"
        );
    }

    let arguments = vec!["x".to_owned(); MAX_SERVER_ARGUMENTS + 1];
    let result = parse_user_configuration(&config_with_arguments(&arguments));
    assert!(matches!(result, Err(ConfigError::InvalidArguments)));
}

#[test]
fn parser_should_enforce_total_argument_bytes_below_at_and_above_limit() {
    let argument_count = MAX_SERVER_ARGUMENTS;
    let at_argument_size = MAX_TOTAL_SERVER_ARGUMENT_BYTES / argument_count;
    assert_eq!(
        at_argument_size * argument_count,
        MAX_TOTAL_SERVER_ARGUMENT_BYTES
    );

    let below = vec!["x".repeat(at_argument_size); argument_count - 1];
    let at = vec!["x".repeat(at_argument_size); argument_count];
    let mut above = at.clone();
    above[0].push('x');

    assert!(parse_user_configuration(&config_with_arguments(&below)).is_ok());
    assert!(parse_user_configuration(&config_with_arguments(&at)).is_ok());
    assert!(matches!(
        parse_user_configuration(&config_with_arguments(&above)),
        Err(ConfigError::InvalidArguments)
    ));
}

#[test]
fn parser_should_reject_startup_timeout_below_conservative_minimum() {
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        "startup_timeout_ms = 99",
    );

    let result = parse_user_configuration(&document);

    assert!(matches!(result, Err(ConfigError::InvalidTimeout { ref id }) if id == "fixture"));
}

#[test]
fn parser_should_reject_request_timeout_above_conservative_maximum() {
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        "request_timeout_ms = 300001",
    );

    let result = parse_user_configuration(&document);

    assert!(matches!(result, Err(ConfigError::InvalidTimeout { ref id }) if id == "fixture"));
}

#[test]
fn parser_should_accept_timeout_endpoints() {
    for extra in [
        "startup_timeout_ms = 100\nrequest_timeout_ms = 100",
        "startup_timeout_ms = 300000\nrequest_timeout_ms = 300000",
    ] {
        let result = parse_user_configuration(&config_with_server(
            "fixture",
            "\"fixture\"",
            "server",
            extra,
        ));

        assert!(result.is_ok(), "timeout endpoint should pass: {result:?}");
    }
}

#[test]
fn parser_should_reject_parent_relative_project_root() {
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        "project_root = \"../outside\"",
    );

    let result = parse_user_configuration(&document);

    assert!(matches!(result, Err(ConfigError::InvalidProjectRoot { ref id }) if id == "fixture"));
}

#[cfg(unix)]
#[test]
fn resolution_should_reject_project_root_symlink_that_escapes_workspace() {
    let root = TestDirectory::new("project-root-symlink");
    let workspace = root.directory("workspace");
    let outside = root.directory("outside");
    std::os::unix::fs::symlink(&outside, workspace.join("escaped"))
        .expect("create escaping directory symlink");
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        "project_root = \"escaped\"",
    );
    let configuration = parse_user_configuration(&document).expect("valid syntactic root");

    let result = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", None),
    );

    assert!(matches!(result, Err(ConfigError::InvalidProjectRoot { ref id }) if id == "fixture"));
}

#[test]
fn automatic_resolution_should_prefer_trusted_user_descriptor() {
    let root = TestDirectory::new("precedence");
    let workspace = root.directory("workspace");
    let system_bin = root.directory("system-bin");
    let executable = root.executable("system-bin/custom-server");
    let path = std::env::join_paths([system_bin]).expect("construct PATH");
    let configuration = parse_user_configuration(&config_with_server(
        "custom-rust",
        "\"rs\"",
        "custom-server",
        "",
    ))
    .expect("valid configuration");

    let resolved = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "rs", Some(&path)),
    )
    .expect("resolve trusted descriptor");

    assert_eq!(
        (
            resolved.configuration_id.as_deref(),
            resolved.origin,
            resolved.process.program()
        ),
        (
            Some("custom-rust"),
            ServerOrigin::UserConfiguration,
            executable.as_os_str()
        )
    );
}

#[test]
fn trusted_descriptor_should_preserve_omitted_options_and_settings_as_none() {
    let root = TestDirectory::new("omitted-payloads");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    root.executable("bin/server");
    let path = std::env::join_paths([bin]).expect("construct PATH");
    let configuration =
        parse_user_configuration(&config_with_server("fixture", "\"fixture\"", "server", ""))
            .expect("valid configuration");

    let resolved = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", Some(&path)),
    )
    .expect("resolve descriptor");

    assert_eq!(
        (resolved.initialization_options, resolved.settings),
        (None, None)
    );
}

#[test]
fn trusted_descriptor_should_preserve_explicit_empty_options_and_settings_as_some() {
    let root = TestDirectory::new("empty-payloads");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    root.executable("bin/server");
    let path = std::env::join_paths([bin]).expect("construct PATH");
    let configuration = parse_user_configuration(&config_with_server(
        "fixture",
        "\"fixture\"",
        "server",
        "initialization_options = {}\nsettings = {}",
    ))
    .expect("valid configuration");

    let resolved = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", Some(&path)),
    )
    .expect("resolve descriptor");

    assert_eq!(
        (
            resolved.initialization_options.as_ref(),
            resolved.settings.as_ref()
        ),
        (Some(&serde_json::json!({})), Some(&serde_json::json!({})))
    );
}

#[test]
fn automatic_resolution_should_require_id_for_duplicate_trusted_extension() {
    let root = TestDirectory::new("duplicate-extension");
    let workspace = root.directory("workspace");
    let document = r#"version = 1
[[servers]]
id = "one"
extensions = ["fixture"]
language_id = "one"
program = "one"
[[servers]]
id = "two"
extensions = ["fixture"]
language_id = "two"
program = "two"
"#;
    let configuration = parse_user_configuration(document).expect("valid configuration");

    let result = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", None),
    );

    assert!(matches!(result, Err(ConfigError::AmbiguousExtension)));
}

#[test]
fn explicit_id_should_disambiguate_duplicate_trusted_extension() {
    let root = TestDirectory::new("explicit-id");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    let executable = root.executable("bin/two");
    let path = std::env::join_paths([bin]).expect("construct PATH");
    let document = r#"version = 1
[[servers]]
id = "one"
extensions = ["fixture"]
language_id = "one"
program = "one"
[[servers]]
id = "two"
extensions = ["fixture"]
language_id = "two-language"
program = "two"
"#;
    let configuration = parse_user_configuration(document).expect("valid configuration");
    let request = ResolutionRequest {
        workspace_root: &workspace,
        source_extension: "fixture",
        selection: ServerSelection::Id("TWO"),
        executable_path: Some(&path),
    };

    let resolved = resolve_server(Some(&configuration), request).expect("resolve selected ID");

    assert_eq!(
        (resolved.language_id.as_str(), resolved.process.program()),
        ("two-language", executable.as_os_str())
    );
}

#[test]
fn built_in_resolution_should_map_typescript_react() {
    let root = TestDirectory::new("typescript");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    root.executable("bin/typescript-language-server");
    let path = std::env::join_paths([bin]).expect("construct PATH");

    let resolved = resolve_server(None, automatic_request(&workspace, ".tsx", Some(&path)))
        .expect("resolve TypeScript built-in");

    assert_eq!(
        (
            resolved.configuration_id.as_deref(),
            resolved.language_id.as_str()
        ),
        (Some("typescript"), "typescriptreact")
    );
}

#[test]
fn built_in_resolution_should_map_swift() {
    let root = TestDirectory::new("swift");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    root.executable("bin/sourcekit-lsp");
    let path = std::env::join_paths([bin]).expect("construct PATH");

    let resolved = resolve_server(None, automatic_request(&workspace, "swift", Some(&path)))
        .expect("resolve Swift built-in");

    assert_eq!(
        (
            resolved.configuration_id.as_deref(),
            resolved.language_id.as_str()
        ),
        (Some("swift"), "swift")
    );
}

#[test]
fn automatic_resolution_should_not_guess_ambiguous_header_language() {
    let root = TestDirectory::new("header");
    let workspace = root.directory("workspace");

    let result = resolve_server(None, automatic_request(&workspace, "h", None));

    assert!(matches!(result, Err(ConfigError::ServerNotConfigured)));
}

#[test]
fn explicit_clangd_id_should_supply_language_for_ambiguous_header() {
    let root = TestDirectory::new("header-id");
    let workspace = root.directory("workspace");
    let bin = root.directory("bin");
    root.executable("bin/clangd");
    let path = std::env::join_paths([bin]).expect("construct PATH");
    let request = ResolutionRequest {
        workspace_root: &workspace,
        source_extension: "h",
        selection: ServerSelection::Id("clangd-c"),
        executable_path: Some(&path),
    };

    let resolved = resolve_server(None, request).expect("explicit built-in ID resolves");

    assert_eq!(resolved.language_id, "c");
}

#[test]
fn automatic_discovery_should_ignore_relative_empty_and_workspace_path_entries() {
    let root = TestDirectory::new("poisoned-path");
    let workspace = root.directory("workspace");
    let workspace_bin = root.directory("workspace/bin");
    root.executable("workspace/bin/rust-analyzer");
    let path = std::env::join_paths([PathBuf::from("."), PathBuf::new(), workspace_bin])
        .expect("construct poisoned PATH");

    let result = resolve_server(None, automatic_request(&workspace, "rs", Some(&path)));

    assert!(matches!(
        result,
        Err(ConfigError::ExecutableNotFound { ref id }) if id == "rust"
    ));
}

#[test]
fn automatic_discovery_should_skip_workspace_shadow_and_select_system_server() {
    let root = TestDirectory::new("shadowed-path");
    let workspace = root.directory("workspace");
    let workspace_bin = root.directory("workspace/bin");
    let system_bin = root.directory("system-bin");
    root.executable("workspace/bin/rust-analyzer");
    let system_server = root.executable("system-bin/rust-analyzer");
    let path = std::env::join_paths([workspace_bin, system_bin]).expect("construct PATH");

    let resolved = resolve_server(None, automatic_request(&workspace, "rs", Some(&path)))
        .expect("select non-workspace executable");

    assert_eq!(resolved.process.program(), system_server.as_os_str());
}

#[test]
fn trusted_descriptor_should_require_opt_in_for_relative_workspace_program() {
    let root = TestDirectory::new("relative-denied");
    let workspace = root.directory("workspace");
    root.executable("workspace/bin/server");
    let configuration = parse_user_configuration(&config_with_server(
        "fixture",
        "\"fixture\"",
        "bin/server",
        "",
    ))
    .expect("valid configuration");

    let result = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", None),
    );

    assert!(matches!(
        result,
        Err(ConfigError::ExecutableNotFound { .. })
    ));
}

#[test]
fn trusted_descriptor_should_allow_opted_in_relative_workspace_program() {
    let root = TestDirectory::new("relative-allowed");
    let workspace = root.directory("workspace");
    let executable = root.executable("workspace/bin/server");
    let configuration = parse_user_configuration(&config_with_server(
        "fixture",
        "\"fixture\"",
        "bin/server",
        "allow_workspace_program = true",
    ))
    .expect("valid configuration");

    let resolved = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", None),
    )
    .expect("resolve explicitly trusted workspace program");

    assert_eq!(resolved.process.program(), executable.as_os_str());
}

#[test]
fn explicit_program_should_win_without_shell_parsing_or_path_discovery() {
    let root = TestDirectory::new("explicit-program");
    let workspace = root.directory("workspace");
    let arguments = vec!["--stdio".to_owned(), "$(touch nope)".to_owned()];
    let request = ResolutionRequest {
        workspace_root: &workspace,
        source_extension: "unknown",
        selection: ServerSelection::Program {
            program: OsStr::new("server --not-a-shell-command"),
            arguments: &arguments,
            language_id: "fixture",
        },
        executable_path: None,
    };

    let resolved = resolve_server(None, request).expect("resolve direct program");

    assert_eq!(
        (
            resolved.process.program(),
            resolved.configuration_id,
            resolved.origin,
            format!("{:?}", resolved.process).contains("has_current_directory: true")
        ),
        (
            OsStr::new("server --not-a-shell-command"),
            None,
            ServerOrigin::Explicit,
            true
        )
    );
}

#[test]
fn resolution_request_debug_should_redact_program_arguments_and_paths() {
    let workspace = Path::new("/secret/workspace");
    let arguments = vec!["secret-argument".to_owned()];
    let executable_path = OsStr::new("/secret/bin");
    let request = ResolutionRequest {
        workspace_root: workspace,
        source_extension: "fixture",
        selection: ServerSelection::Program {
            program: OsStr::new("secret-program"),
            arguments: &arguments,
            language_id: "fixture",
        },
        executable_path: Some(executable_path),
    };

    let rendered = format!("{request:?}");

    assert!(
        !rendered.contains("secret-program")
            && !rendered.contains("secret-argument")
            && !rendered.contains("/secret"),
        "sensitive request data leaked through Debug: {rendered}"
    );
}

#[test]
fn resolved_debug_should_redact_program_options_settings_and_absolute_root() {
    let root = TestDirectory::new("redacted-debug");
    let workspace = root.directory("workspace-secret-root");
    let bin = root.directory("bin");
    root.executable("bin/secret-program-name");
    let path = std::env::join_paths([bin]).expect("construct PATH");
    let document = config_with_server(
        "fixture",
        "\"fixture\"",
        "secret-program-name",
        "initialization_options = { token = \"initialization-secret\" }\nsettings = { token = \"settings-secret\" }",
    );
    let configuration = parse_user_configuration(&document).expect("valid configuration");
    let resolved = resolve_server(
        Some(&configuration),
        automatic_request(&workspace, "fixture", Some(&path)),
    )
    .expect("resolve descriptor");

    let rendered = format!("{resolved:?}");

    assert!(
        !rendered.contains("secret-program-name")
            && !rendered.contains("initialization-secret")
            && !rendered.contains("settings-secret")
            && !rendered.contains("workspace-secret-root"),
        "sensitive configuration leaked through Debug: {rendered}"
    );
}

#[test]
fn error_display_should_not_echo_paths_or_program_values() {
    let error = ConfigError::ExecutableNotFound {
        id: "safe-id".to_owned(),
    };

    let rendered = error.to_string();

    assert_eq!(
        rendered,
        "language-server executable for configuration `safe-id` was not found in a trusted location"
    );
}

#[test]
fn path_input_should_accept_owned_os_string_without_environment_access() {
    let override_path = OsString::from("custom.toml");

    let result = configuration_path(Some(&override_path), None);

    assert_eq!(result, Some(PathBuf::from("custom.toml")));
}
