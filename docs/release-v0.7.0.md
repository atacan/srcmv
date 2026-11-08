# srcmv v0.7.0 release notes

This minor release adds Swift to the built-in language-server descriptors used
by the read-only `select` and `outline` commands, and makes the bundled agent
skill self-contained for language-server setup.

## Highlights

- Built-in `swift` descriptor. `.swift` files now resolve automatically to an
  installed `sourcekit-lsp` (language ID `swift`) found on a trusted absolute
  `PATH` entry. On macOS with Xcode or the Command Line Tools,
  `/usr/bin/sourcekit-lsp` qualifies, so no `--server-program` or user
  configuration is needed. As with every built-in, the server is not bundled.
- Agent skill: `skills/srcmv/references/semantic-selection.md` now documents
  the built-in descriptor table, explicit `--server-program` /
  `--language-id` / `--server-arg` usage, the user configuration file location
  and format, and Swift-specific naming. sourcekit-lsp reports functions,
  methods, and initializers by full Swift name (`isControl(_:)`,
  `init(name:)`), protocols as `interface`, and extensions as `namespace`.

## Compatibility

No protocol-v1 edit contract, selection-v1 or outline-v1 schema, or error code
changes. Selection on other languages is unchanged. On earlier versions, Swift
selection remains available through `--server-program "$(xcrun --find
sourcekit-lsp)" --language-id swift` or a user descriptor.

## Qualification

Swift selection was smoke-tested against sourcekit-lsp from Xcode 27.2 on
macOS arm64, on a SwiftPM package and on a loose `.swift` file.
