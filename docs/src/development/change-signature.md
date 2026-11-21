---
title: Change Signature
description: "Use and develop the experimental Change Signature refactoring for C++ and Rust."
---

# Change Signature

Change Signature updates a function or method and its resolved references in the active project. It requires the matching development build of clangd or rust-analyzer.

## Use the refactoring

1. Place the cursor on a function name in a declaration or direct call.
2. Run {#action editor::ChangeSignature} from the command palette, or choose **Change Signature** from the code-action menu.
3. Edit the ordered `parameters` list in the signature form. Keep an existing parameter's `oldIndex` when moving or editing it. Delete an entry to remove that parameter. Set `oldIndex` to `null` for a new parameter and provide an `argument` expression for existing calls.
4. Choose **Continue** to open the preview tab. Review the file diffs and server warnings, then choose **Apply** in the top-right toolbar. Choose **Cancel** or close the preview tab to discard it.

The form includes a read-only JSON example. With a compatible clangd, **Force** opens a preview even when the proposed C++ edits produce compiler errors. The preview shows the compiler diagnostic and requires explicit confirmation. Reference resolution and buffer validation still apply.

The first version uses an editable JSON form. For example, moving the second parameter before the first and adding a Boolean parameter in C++ looks like this:

```json
{
  "name": "calculate",
  "parameters": [
    { "oldIndex": 1, "name": "y", "type": "int" },
    { "oldIndex": 0, "name": "x", "type": "int" },
    {
      "oldIndex": null,
      "name": "normalize",
      "type": "bool",
      "argument": "false"
    }
  ],
  "returnType": "int"
}
```

On Linux, Control+Enter submits the form and Enter confirms the preview. The editor action has no default shortcut. You can bind `editor::ChangeSignature` in your keymap.

## Run local builds

Build Zed using the [Linux development instructions](./linux.md). Build clangd from the `feature/change-signature` branch of the LLVM fork. Use a build directory configured with `LLVM_ENABLE_PROJECTS="clang;clang-tools-extra"`.

```sh
cmake --build /path/to/llvm-build --target clangd
```

Build rust-analyzer from its `feature/change-signature` branch using the toolchain required by that checkout.

```sh
cargo build --locked -p rust-analyzer
```

In a trusted test project's `.zed/settings.json`, use the absolute paths to the binaries.

```json
{
  "lsp": {
    "clangd": {
      "binary": {
        "path": "/path/to/llvm-build/bin/clangd",
        "arguments": ["--background-index"]
      }
    },
    "rust-analyzer": {
      "binary": {
        "path": "/path/to/rust-analyzer/target/debug/rust-analyzer",
        "arguments": []
      }
    }
  }
}
```

For a C++ project, provide `compile_commands.json` and wait for clangd to index the project. For a Rust project, open the directory containing `Cargo.toml` and wait for rust-analyzer to load its crate graph. Restart the language server after changing its binary path.

To run the executable protocol fixtures from the Zed checkout, pass both binaries to the test script.

```sh
script/test-change-signature \
  --clangd /path/to/llvm-build/bin/clangd \
  --rust-analyzer /path/to/rust-analyzer/target/debug/rust-analyzer
```

## Protocol and edit application

The client follows the [gopls interactive refactoring protocol](https://go.googlesource.com/tools/+/refs/heads/master/gopls/doc/design/integrating-interactive-refactoring.md) for capability discovery and form resolution. It advertises `experimental.interactiveResolve.inputTypes`. A compatible server advertises `experimental.interactiveResolveProvider.kinds` with `"command"`. A `refactor.rewrite` code action provides the `refactor.changeSignature` command. Zed sends `command/resolve`, collects answers and displays validation errors, then sends `workspace/executeCommand` when no fields remain.

The preview extension adds `workspaceEditPreview: true` to the client's interactive capability and lists previewable commands under the server's `interactiveResolveProvider.previewCommands`. For those commands, execution returns `{ edit, originals, warnings }`. The `edit` is a standard LSP `WorkspaceEdit`. The `originals` object maps each edited URI to the source text the server used to calculate its edits. The server never sends `workspace/applyEdit` for Change Signature. A server can advertise commands that accept `force: true` in `command/resolve` via `interactiveResolveProvider.forcePreviewCommands`. The signature itself is a standard string field named `signature`, with JSON carrying the ordered parameter identities and optional return type. A Rust function without an explicit return type uses `null`.

Zed resolves UTF-16 ranges against the actual buffers, shows the changes in a read-only diff tab, and checks every staged buffer again before editing any of them. Successful changes use Zed's project transaction path for multi-file undo. The editor workflow is in `crates/editor/src/interactive_refactor.rs` and the protocol types are in `crates/lsp/src/interactive_refactor.rs`.

## Language server behavior

- **C++.** `clang-tools-extra/clangd/refactor/ChangeSignature.cpp` in the LLVM fork resolves the function with Clang's AST, finds redeclarations, overrides and reference files through clangd's index, and parses those files with their compilation settings. It reparses the affected translation units against the proposed edits before returning the preview.
- **Rust.** `crates/ide/src/change_signature.rs` in the rust-analyzer fork uses HIR definitions and usage search to find functions, trait implementations and call sites. It edits syntax nodes while preserving the receiver in method and fully qualified call syntax. Type and return edits can still require manual changes. Review the warnings and run `cargo check` after applying them.

Both servers support adding parameters with an explicit call-site expression, removing unused parameters, reordering when evaluation is safe, and renaming ordinary parameter bindings. Coverage is limited to active source the server can resolve. Code in inactive configurations, other build variants, and downstream projects can require separate updates.

The servers reject unsupported references and cases such as indirect function values, unsafe argument moves, overlapping edits, and source they cannot map back from a macro. Clangd also rejects ambiguous overrides, potentially affected overload resolution, and complex declarators such as templates and function pointers. Rust currently requires ordinary named bindings and cannot update declarations in dependencies. Zed supports static text, Boolean, numeric, and enum fields in the interactive flow. Dynamic enums, lists, file pickers, remote projects, and file creation or renaming in preview edits are outside this version's scope.
