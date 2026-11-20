---
title: Finding and Navigating Code - Zed
description: Navigate your codebase in Zed with file finder, project search, go to definition, symbol search, and the command palette.
---

# Finding & Navigating

Zed provides several ways to move around your codebase quickly. Here's an overview of the main navigation tools.

## Command Palette

The Command Palette ({#kb command_palette::Toggle}) is your gateway to almost everything in Zed. Type a few characters to filter commands, then press Enter to execute.

[Learn more about the Command Palette →](./command-palette.md)

## Project Panel

The Project Panel ({#kb project_panel::ToggleFocus}) shows a tree view of your workspace's files and directories. Browse, create, rename, move, and delete files without leaving the editor. It also surfaces git status and diagnostics at a glance.

[Learn more about the Project Panel →](./project-panel.md)

## File Finder

Open any file in your project with {#kb file_finder::Toggle}. Type part of the filename or path to narrow results.

## Text Finder

Quickly find any string in your project and open the file with {#kb project_search::OpenTextFinder}. Changed your mind and want a more detailed search with extra filters? Move to the project search using the button in the Actions menu in the right bottom corner.

## Project Search

Search across all files with {#kb pane::DeploySearch}. Type the query in the search field, then press Enter to run the search.

Results appear in a [multibuffer](./multibuffers.md), letting you edit matches in place.

## Go to Definition

Jump to where a symbol is defined with {#kb editor::GoToDefinition} (or `Cmd+Click` / `Ctrl+Click`). If there are multiple definitions, they open in a multibuffer.

## Go to Symbol

- **Current file:** {#kb outline::Toggle} opens an outline of symbols in the active file
- **Entire project:** {#kb project_symbols::Toggle} searches symbols across all files

## Project Bookmarks {#project-bookmarks}

Run {#action project_bookmarks::Toggle} from the [command palette](./command-palette.md) to browse bookmarks across your project. Type to search bookmark labels; bookmarks without a label use the bookmarked line's text. Each result shows the file path and line number. Click the Preview button to toggle the code preview, then confirm a result to jump to its location.

This command has no default keybinding. You can assign one in your [keymap](./key-bindings.md).

## Call Hierarchy

With the cursor on a function, {#action call_hierarchy::ShowIncomingCalls} ({#kb call_hierarchy::ShowIncomingCalls}) lists the functions that call it, and {#action call_hierarchy::ShowOutgoingCalls} lists the functions it calls.
Both open a picker; selecting an entry jumps to the call site, and the secondary confirm opens it in an adjacent pane.
While the picker is open, {#action call_hierarchy::ToggleDirection} switches between callers and callees, {#action menu::SelectChild} re-roots the hierarchy at the selected function to explore one level deeper, and {#action menu::SelectParent} steps back up; the picker's footer shows the corresponding key bindings.
Both actions are also available in the editor's right-click menu.
The modal's width can be changed via the `call_hierarchy.modal_max_width` setting.
This requires the language server to support call hierarchy.

## Type Hierarchy

With the cursor on a type, {#action type_hierarchy::ShowSupertypes} lists its base types and {#action type_hierarchy::ShowSubtypes} lists its derived types. The picker uses the same controls as Call Hierarchy. Type Hierarchy is available in local projects with a language server that supports the standard requests, such as clangd.

## Hierarchy Results {#hierarchy-results}

Choose {#action call_hierarchy::OpenHierarchyInResultsTab} from either picker’s footer or the command palette, or press {#kb call_hierarchy::OpenHierarchyInResultsTab}. The tab retains the picker’s loaded results, selection, and path in the current direction. If the path includes steps in the opposite direction, the tree starts after those steps.

To skip the picker, run one of these commands from the [command palette](./command-palette.md):

- {#action call_hierarchy::ShowIncomingCallsInResults}
- {#action call_hierarchy::ShowOutgoingCallsInResults}
- {#action type_hierarchy::ShowSupertypesInResults}
- {#action type_hierarchy::ShowSubtypesInResults}

Use the up and down arrows to select a row. The right arrow expands it or moves to its first child. The left arrow collapses it or moves to its parent. In Vim mode, you can also use `j`, `k`, `l`, and `h`. Press Enter or double-click to open the definition or the individual call site in a separate pane. Expand a call to see its call sites alongside related symbols.

The direction button switches between callers and callees or between supertypes and subtypes. Each direction remembers its expanded branches and selection. Zed fetches branches when you expand them and reuses the results when you reopen them. You can move or split hierarchy tabs like other tabs. They stay open while you edit and navigate during the current session.

## Outline Panel

The Outline Panel ({#kb outline_panel::ToggleFocus}) shows a persistent tree view of symbols in the current file. It's especially useful with [multibuffers](./multibuffers.md) for navigating search results or diagnostics.

[Learn more about the Outline Panel →](./outline-panel.md)

## Tab Switcher

Quickly switch between open tabs with {#kb tab_switcher::Toggle}. Tabs are sorted by recent use—keep holding Ctrl and press Tab to cycle through them.

[Learn more about the Tab Switcher →](./tab-switcher.md)

## Quick Reference

| Task               | Keybinding                           |
| ------------------ | ------------------------------------ |
| Command Palette    | {#kb command_palette::Toggle}        |
| Open file          | {#kb file_finder::Toggle}            |
| Project search     | {#kb pane::DeploySearch}             |
| Text search picker | {#kb project_search::OpenTextFinder} |
| Go to definition   | {#kb editor::GoToDefinition}         |
| Find references    | {#kb editor::FindAllReferences}      |
| Symbol in file     | {#kb outline::Toggle}                |
| Symbol in project  | {#kb project_symbols::Toggle}        |
| Outline Panel      | {#kb outline_panel::ToggleFocus}     |
| Tab Switcher       | {#kb tab_switcher::Toggle}           |
| Project Panel      | {#kb project_panel::ToggleFocus}     |
