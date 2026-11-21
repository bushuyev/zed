use std::collections::{HashMap, HashSet};

use futures::channel::oneshot;
use gpui::DismissEvent;
use lsp::{
    Edit, LanguageServer,
    interactive_refactor::{self, Answer, Field, FieldKind, InteractiveCommand, PreviewEdit},
};
use serde_json::Value;

use super::*;
use workspace::{DismissDecision, ModalView};

impl Editor {
    pub(crate) fn change_signature_available(&self, cx: &App) -> bool {
        if !self.mode().is_full() || self.read_only(cx) {
            return false;
        }
        let Some(project) = self.project() else {
            return false;
        };
        if !project.read(cx).is_local() {
            return false;
        }
        let store = project.read(cx).lsp_store();
        let selection = self.selections.newest_anchor().head();
        let Some((buffer, _)) = self
            .buffer()
            .read(cx)
            .text_anchor_for_position(selection, cx)
        else {
            return false;
        };
        let store = store.read(cx);
        store
            .language_server_ids_for_opened_buffer(buffer.read(cx).remote_id())
            .into_iter()
            .flatten()
            .filter_map(|id| store.language_server_for_id(*id))
            .any(|server| {
                interactive_refactor::supports_preview(
                    &server.capabilities(),
                    interactive_refactor::CHANGE_SIGNATURE,
                )
            })
    }

    pub(crate) fn change_signature(
        &mut self,
        _: &ChangeSignature,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace().map(|workspace| workspace.downgrade()) else {
            return;
        };
        let point = self.selections.newest_anchor().head();
        let Some((buffer, position)) = self.buffer().read(cx).text_anchor_for_position(point, cx)
        else {
            return;
        };
        let task = project.update(cx, |project, cx| {
            project.code_actions(
                &buffer,
                position..position,
                Some(vec![lsp::CodeActionKind::REFACTOR]),
                cx,
            )
        });
        cx.spawn_in(window, async move |editor, cx| {
            let actions = match task.await {
                Ok(actions) => actions.unwrap_or_default(),
                Err(error) => {
                    workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
                    return Ok(());
                }
            };
            let target = project.read_with(cx, |project, cx| {
                actions.iter().find_map(|action| {
                    let command = action.lsp_action.command()?;
                    if command.command != interactive_refactor::CHANGE_SIGNATURE {
                        return None;
                    }
                    let server = interactive_server(project, action, cx)?;
                    interactive_refactor::supports_preview(&server.capabilities(), &command.command)
                        .then_some((action, server))
                })
            });
            let Some((action, server)) = target else {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_error(
                        anyhow!("Change Signature is unavailable at this cursor position"),
                        cx,
                    );
                })?;
                return Ok(());
            };
            run(&editor, workspace, project, buffer, action, server, cx).await
        })
        .detach_and_log_err(cx);
    }
}

pub(super) fn interactive_server(
    project: &Project,
    action: &CodeAction,
    cx: &App,
) -> Option<Arc<LanguageServer>> {
    let command = action.lsp_action.command()?;
    if !project.is_local() || action.lsp_action.edit().is_some() {
        return None;
    }
    let server = project
        .lsp_store()
        .read(cx)
        .language_server_for_id(action.server_id)?;
    let capabilities = server.capabilities();
    if !interactive_refactor::supports_resolution(&capabilities)
        || !capabilities
            .execute_command_provider
            .as_ref()
            .is_some_and(|provider| provider.commands.contains(&command.command))
    {
        return None;
    }
    Some(server)
}

pub(super) async fn run(
    editor: &WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    action: &CodeAction,
    server: Arc<LanguageServer>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let result = run_command(editor, &workspace, project, buffer, action, server, cx).await;
    if let Err(error) = result {
        workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
    }
    Ok(())
}

async fn run_command(
    editor: &WeakEntity<Editor>,
    workspace: &WeakEntity<Workspace>,
    project: Entity<Project>,
    source: Entity<Buffer>,
    action: &CodeAction,
    server: Arc<LanguageServer>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let original_command = action
        .lsp_action
        .command()
        .context("Missing interactive command")?;
    let title = action.lsp_action.title().to_string();
    let preview =
        interactive_refactor::supports_preview(&server.capabilities(), &original_command.command);
    anyhow::ensure!(
        preview || original_command.command != interactive_refactor::CHANGE_SIGNATURE,
        "The language server does not support previewing Change Signature"
    );
    let version = source.read_with(cx, |buffer, _| buffer.version());
    let mut command = InteractiveCommand::from(original_command);
    let mut answers = HashMap::<String, Value>::new();
    let timeout = cx.update(|_, cx| {
        project::project_settings::ProjectSettings::get_global(cx)
            .global_lsp_settings
            .get_request_timeout()
    })?;

    loop {
        command = server
            .request::<interactive_refactor::ResolveCommand>(command, timeout)
            .await
            .into_response()
            .context("Resolving refactoring command")?;
        anyhow::ensure!(
            command.command == original_command.command,
            "The language server changed the command being resolved"
        );
        if command.form_fields.is_empty() {
            break;
        }

        for answer in &command.form_answers {
            answers.insert(answer.id.clone(), answer.value.clone());
        }
        let mut field_ids = HashSet::new();
        let mut collected = Vec::with_capacity(command.form_fields.len());
        for field in std::mem::take(&mut command.form_fields) {
            anyhow::ensure!(
                field_ids.insert(field.id.clone()),
                "The refactoring form contains duplicate field IDs"
            );
            let current = answers
                .get(&field.id)
                .cloned()
                .unwrap_or_else(|| field.default.clone());
            let Some(answer) = show_form(workspace, &title, field, current, cx).await? else {
                return Ok(());
            };
            answers.insert(answer.id.clone(), answer.value.clone());
            if !answer.value.is_null() {
                collected.push(answer);
            }
        }
        command.form_answers = collected;
    }

    anyhow::ensure!(
        source.read_with(cx, |buffer, _| buffer.version()) == version,
        "The original document changed. Start the refactoring again."
    );
    let response = server
        .request::<interactive_refactor::ExecuteCommand>(command, timeout)
        .await
        .into_response()
        .context("Executing refactoring command")?;
    if !preview {
        return Ok(());
    }
    let proposal: PreviewEdit =
        serde_json::from_value(response.context("The language server did not return a preview")?)?;
    let staged = stage_preview(&project, proposal, cx).await?;
    if staged.files.is_empty() {
        return Ok(());
    }
    let confirmed = show_preview(workspace, &title, staged.diff, cx).await?;
    if confirmed {
        let transaction = cx.update(|_, cx| {
            anyhow::ensure!(
                source.read(cx).version() == version,
                "The original document changed. Start the refactoring again."
            );
            apply_preview(staged.files, cx)
        })??;
        Editor::open_project_transaction(editor, workspace.clone(), transaction, title, cx).await?;
    }
    Ok(())
}

struct StagedPreview {
    files: Vec<StagedFile>,
    diff: String,
}

struct StagedFile {
    buffer: Entity<Buffer>,
    version: clock::Global,
    edits: Vec<(Range<usize>, String)>,
}

async fn stage_preview(
    project: &Entity<Project>,
    proposal: PreviewEdit,
    cx: &mut AsyncWindowContext,
) -> Result<StagedPreview> {
    let PreviewEdit {
        edit,
        mut originals,
        warnings,
    } = proposal;
    let mut changes = HashMap::<lsp::Uri, Vec<lsp::TextEdit>>::new();
    match (edit.changes, edit.document_changes) {
        (Some(edits), None) => changes.extend(edits),
        (None, Some(lsp::DocumentChanges::Edits(edits))) => {
            for edit in edits {
                insert_document_edit(&mut changes, edit)?;
            }
        }
        (None, Some(lsp::DocumentChanges::Operations(operations))) => {
            for operation in operations {
                let lsp::DocumentChangeOperation::Edit(edit) = operation else {
                    anyhow::bail!("Refactoring previews support text edits only");
                };
                insert_document_edit(&mut changes, edit)?;
            }
        }
        (None, None) => {}
        (Some(_), Some(_)) => anyhow::bail!("The language server returned ambiguous edit formats"),
    }
    let mut diff = warnings.join("\n");
    if !diff.is_empty() {
        diff.push_str("\n\n");
    }
    let mut changes = changes.into_iter().collect::<Vec<_>>();
    changes.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
    let mut files = Vec::new();
    let mut opened = HashSet::new();
    for (uri, edits) in changes {
        let original = originals
            .remove(&uri)
            .context("The server did not supply the original text of an edited file")?;
        let original = original.replace("\r\n", "\n");
        let path = uri
            .to_file_path()
            .map_err(|_| anyhow!("The refactoring contains a non-file URI"))?;
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(&path, cx))
            .await?;
        anyhow::ensure!(
            opened.insert(buffer.entity_id()),
            "The refactoring addresses the same file by multiple URIs"
        );
        let (version, edits) = buffer.read_with(cx, |buffer, _| {
            anyhow::ensure!(
                buffer.capability().editable() && !buffer.has_conflict(),
                "Cannot edit {}",
                path.display()
            );
            anyhow::ensure!(
                original == buffer.text(),
                "{} changed. Start the refactoring again.",
                path.display()
            );
            let edits = edit_offsets(buffer, edits)?;
            Ok::<_, anyhow::Error>((buffer.version(), edits))
        })?;

        let mut modified = original.clone();
        for (range, replacement) in edits.iter().rev() {
            modified.replace_range(range.clone(), replacement);
        }
        if modified != original {
            use std::fmt::Write as _;
            writeln!(diff, "--- {}\n+++ {}", path.display(), path.display())?;
            diff.push_str(&language::unified_diff(&original, &modified));
            diff.push('\n');
            files.push(StagedFile {
                buffer,
                version,
                edits,
            });
        }
    }
    Ok(StagedPreview { files, diff })
}

fn insert_document_edit(
    changes: &mut HashMap<lsp::Uri, Vec<lsp::TextEdit>>,
    edit: lsp::TextDocumentEdit,
) -> Result<()> {
    let edits = edit
        .edits
        .into_iter()
        .map(|edit| match edit {
            Edit::Plain(edit) => Ok(edit),
            Edit::Annotated(edit) => Ok(edit.text_edit),
            _ => Err(anyhow!(
                "Refactoring previews support plain text edits only"
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        changes.insert(edit.text_document.uri, edits).is_none(),
        "The refactoring has multiple operations for the same document"
    );
    Ok(())
}

fn edit_offsets(buffer: &Buffer, edits: Vec<lsp::TextEdit>) -> Result<Vec<(Range<usize>, String)>> {
    let mut offsets = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = language::range_from_lsp(edit.range);
        let start = buffer.clip_point_utf16(range.start, Bias::Left);
        let end = buffer.clip_point_utf16(range.end, Bias::Left);
        anyhow::ensure!(
            start == range.start.0 && end == range.end.0 && start <= end,
            "The language server returned an invalid text range"
        );
        offsets.push((
            text::ToOffset::to_offset(&start, buffer)..text::ToOffset::to_offset(&end, buffer),
            edit.new_text.replace("\r\n", "\n"),
        ));
    }
    offsets.sort_by_key(|(range, _)| (range.start, range.end));
    anyhow::ensure!(
        offsets
            .windows(2)
            .all(|pair| { pair[0].0.start != pair[1].0.start && pair[0].0.end <= pair[1].0.start }),
        "The language server returned conflicting edits"
    );
    Ok(offsets)
}

fn apply_preview(files: Vec<StagedFile>, cx: &mut App) -> Result<ProjectTransaction> {
    for file in &files {
        let buffer = file.buffer.read(cx);
        anyhow::ensure!(
            buffer.capability().editable()
                && !buffer.has_conflict()
                && buffer.version() == file.version,
            "A previewed document changed. Start the refactoring again."
        );
    }
    let mut transaction = ProjectTransaction::default();
    for file in files {
        let finalized = file.buffer.update(cx, |buffer, cx| {
            buffer.finalize_last_transaction();
            buffer.start_transaction();
            buffer.edit(file.edits, None, cx);
            buffer.end_transaction(cx)?;
            buffer.finalize_last_transaction().cloned()
        });
        if let Some(finalized) = finalized {
            transaction.0.insert(file.buffer, finalized);
        }
    }
    Ok(transaction)
}

async fn show_form(
    workspace: &WeakEntity<Workspace>,
    title: &str,
    field: Field,
    initial_value: Value,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Answer>> {
    anyhow::ensure!(
        !matches!(field.kind, FieldKind::Unsupported),
        "This refactoring requires a form field Zed does not support"
    );
    let (response, answer) = oneshot::channel();
    let title: SharedString = title.to_owned().into();
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                let text = initial_value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if initial_value.is_null() {
                            String::new()
                        } else {
                            initial_value.to_string()
                        }
                    });
                let input = cx.new(|cx| {
                    let mut editor = Editor::multi_line(window, cx);
                    editor.set_text(text, window, cx);
                    editor
                });
                RefactorForm {
                    title,
                    field,
                    input,
                    response: Some(response),
                    error: None,
                }
            });
        })
    })??;
    Ok(answer.await.ok())
}

struct RefactorForm {
    title: SharedString,
    field: Field,
    input: Entity<Editor>,
    response: Option<oneshot::Sender<Answer>>,
    error: Option<SharedString>,
}

impl RefactorForm {
    fn submit(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        match parse_answer(&self.field, self.input.read(cx).text(cx)) {
            Ok(value) => {
                if let Some(response) = self.response.take() {
                    // The editor task may have already been closed.
                    response
                        .send(Answer {
                            id: self.field.id.clone(),
                            value,
                        })
                        .ok();
                }
                cx.emit(DismissEvent);
            }
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }
}

fn parse_answer(field: &Field, text: String) -> Result<Value> {
    let value = text.trim();
    if value.is_empty() {
        anyhow::ensure!(!field.required, "This field is required");
        return Ok(Value::Null);
    }
    match &field.kind {
        FieldKind::String => Ok(Value::String(text)),
        FieldKind::Bool => Ok(Value::Bool(
            value.parse::<bool>().context("Enter true or false")?,
        )),
        FieldKind::Number => Ok(Value::Number(
            value.parse().context("Enter a valid number")?,
        )),
        FieldKind::Enum { entries } => {
            anyhow::ensure!(
                entries.iter().any(|entry| entry.value == value),
                "Enter one of the listed values"
            );
            Ok(Value::String(value.to_owned()))
        }
        FieldKind::Unsupported => anyhow::bail!("Unsupported interactive field"),
    }
}

impl Focusable for RefactorForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for RefactorForm {}
impl ModalView for RefactorForm {
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        self.response.take();
        DismissDecision::Dismiss(true)
    }
}
impl Render for RefactorForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let options = match &self.field.kind {
            FieldKind::Enum { entries } => Some(
                entries
                    .iter()
                    .map(|entry| format!("{} — {}", entry.value, entry.description))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            FieldKind::Bool => Some("Enter true or false".to_owned()),
            _ => None,
        };
        v_flex()
            .key_context("InteractiveRefactor")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .shadow_lg()
            .w(rems(50.))
            .max_w_full()
            .p_4()
            .gap_2()
            .child(Label::new(self.title.clone()).size(LabelSize::Large))
            .child(Label::new(self.field.description.clone()))
            .children(options.map(Label::new))
            .children(
                self.error
                    .clone()
                    .or_else(|| self.field.error.clone().map(Into::into))
                    .map(|error| Label::new(error).color(Color::Error)),
            )
            .child(div().h(rems(17.)).child(self.input.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(Button::new("continue", "Continue").on_click(
                        cx.listener(|this, _, window, cx| this.submit(&menu::Confirm, window, cx)),
                    )),
            )
    }
}

async fn show_preview(
    workspace: &WeakEntity<Workspace>,
    title: &str,
    diff: String,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let (response, confirm) = oneshot::channel();
    let title = format!("{title} Preview").into();
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::multi_line(window, cx);
                    editor.set_use_modal_editing(false);
                    editor
                        .set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
                    editor.set_text(diff, window, cx);
                    editor.move_to_beginning(&MoveToBeginning, window, cx);
                    editor.set_read_only(true);
                    editor
                });
                RefactorPreview {
                    editor,
                    title,
                    response: Some(response),
                }
            })
        })
    })??;
    Ok(confirm.await.is_ok())
}

struct RefactorPreview {
    editor: Entity<Editor>,
    title: SharedString,
    response: Option<oneshot::Sender<()>>,
}

impl RefactorPreview {
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(response) = self.response.take() {
            response.send(()).ok();
        }
        cx.emit(DismissEvent);
    }
}

impl Focusable for RefactorPreview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for RefactorPreview {}
impl ModalView for RefactorPreview {
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        self.response.take();
        DismissDecision::Dismiss(true)
    }
}
impl Render for RefactorPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RefactorPreview")
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .shadow_lg()
            .w(rems(70.))
            .max_w_full()
            .p_4()
            .gap_2()
            .child(Label::new(self.title.clone()).size(LabelSize::Large))
            .child(div().h(rems(25.)).child(self.editor.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("apply", "Apply")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;
    use crate::test::editor_lsp_test_context::EditorLspTestContext;

    #[gpui::test]
    async fn test_change_signature_collects_answers_previews_and_can_cancel(
        cx: &mut TestAppContext,
    ) {
        let advertised = lsp::ServerCapabilities {
            code_action_provider: Some(lsp::CodeActionProviderCapability::Options(
                lsp::CodeActionOptions {
                    code_action_kinds: Some(vec![lsp::CodeActionKind::REFACTOR]),
                    ..Default::default()
                },
            )),
            execute_command_provider: Some(lsp::ExecuteCommandOptions {
                commands: vec![interactive_refactor::CHANGE_SIGNATURE.to_owned()],
                ..Default::default()
            }),
            experimental: Some(json!({
                "interactiveResolveProvider": {
                    "kinds": ["command"],
                    "previewCommands": [interactive_refactor::CHANGE_SIGNATURE],
                }
            })),
            ..Default::default()
        };
        let mut test = EditorLspTestContext::new_rust(advertised, cx).await;
        let before =
            "fn calculate(x: i32, y: i32) -> i32 { x - y }\nfn main() { calculate(10, 3); }\n";
        let after =
            "fn calculate(y: i32, x: i32) -> i32 { x - y }\nfn main() { calculate(3, 10); }\n";
        test.cx.set_state(&format!("ˇ{before}"));
        test.cx.executor().run_until_parked();

        test.lsp
            .set_request_handler::<lsp::request::CodeActionRequest, _, _>(|_, _| async {
                Ok(Some(vec![lsp::CodeActionOrCommand::CodeAction(
                    lsp::CodeAction {
                        title: "Change Signature".into(),
                        kind: Some(lsp::CodeActionKind::REFACTOR_REWRITE),
                        command: Some(lsp::Command {
                            title: "Change Signature".into(),
                            command: interactive_refactor::CHANGE_SIGNATURE.into(),
                            arguments: None,
                        }),
                        ..Default::default()
                    },
                )]))
            });
        test.lsp
            .set_request_handler::<interactive_refactor::ResolveCommand, _, _>(
                |mut command, _| async move {
                    if command
                        .form_answers
                        .first()
                        .is_none_or(|answer| answer.value != "reorder")
                    {
                        command.form_fields = vec![Field {
                            id: "signature".into(),
                            description: "Edit parameters".into(),
                            kind: FieldKind::String,
                            required: true,
                            default: json!("original"),
                            error: (!command.form_answers.is_empty()).then(|| "Try again".into()),
                        }];
                    }
                    Ok(command)
                },
            );
        let executions = Arc::new(AtomicUsize::new(0));
        test.lsp
            .set_request_handler::<interactive_refactor::ExecuteCommand, _, _>({
                let executions = executions.clone();
                let uri = test.buffer_lsp_url.clone();
                move |command, _| {
                    assert_eq!(command.form_answers[0].value, "reorder");
                    executions.fetch_add(1, Ordering::SeqCst);
                    let uri = uri.clone();
                    async move {
                        Ok(Some(serde_json::to_value(PreviewEdit {
                            edit: lsp::WorkspaceEdit {
                                changes: Some(
                                    [(
                                        uri.clone(),
                                        vec![lsp::TextEdit {
                                            range: lsp::Range::new(
                                                lsp::Position::new(0, 0),
                                                lsp::Position::new(2, 0),
                                            ),
                                            new_text: after.into(),
                                        }],
                                    )]
                                    .into_iter()
                                    .collect(),
                                ),
                                ..Default::default()
                            },
                            originals: [(uri, before.to_owned())].into_iter().collect(),
                            warnings: vec!["Workspace callers".into()],
                        })?))
                    }
                }
            });

        let editor = test.cx.editor.clone();
        assert!(editor.read_with(&test.cx.cx, |editor, cx| {
            editor.change_signature_available(cx)
        }));
        test.cx.cx.dispatch_action(test.cx.window, ChangeSignature);
        test.cx.executor().run_until_parked();
        assert!(test.update_workspace(|workspace, _, cx| {
            workspace.active_modal::<RefactorForm>(cx).is_some()
        }));
        test.cx.cx.dispatch_action(test.cx.window, menu::Cancel);
        test.cx.executor().run_until_parked();
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), before);

        test.cx.cx.dispatch_action(test.cx.window, ChangeSignature);
        test.cx.executor().run_until_parked();
        let form = test
            .update_workspace(|workspace, _, cx| workspace.active_modal::<RefactorForm>(cx))
            .expect("signature form");
        form.update_in(&mut test.cx.cx, |form, window, cx| {
            form.submit(&menu::Confirm, window, cx)
        });
        test.cx.executor().run_until_parked();
        let form = test
            .update_workspace(|workspace, _, cx| workspace.active_modal::<RefactorForm>(cx))
            .expect("validation form");
        form.update_in(&mut test.cx.cx, |form, window, cx| {
            assert_eq!(form.field.error.as_deref(), Some("Try again"));
            form.input
                .update(cx, |input, cx| input.set_text("reorder", window, cx));
            form.submit(&menu::Confirm, window, cx);
        });
        test.cx.executor().run_until_parked();
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), before);
        let preview = test
            .update_workspace(|workspace, _, cx| workspace.active_modal::<RefactorPreview>(cx))
            .expect("signature preview");
        preview.read_with(&test.cx.cx, |preview, cx| {
            let diff = preview.editor.read(cx).text(cx);
            assert!(diff.contains("calculate(3, 10)"));
        });
        preview.update(&mut test.cx.cx, |preview, cx| preview.confirm(cx));
        test.cx.executor().run_until_parked();
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), after);
    }

    #[gpui::test]
    fn test_stale_preview_rejects_every_file(cx: &mut App) {
        let first = cx.new(|cx| Buffer::local("first", cx));
        let stale = cx.new(|cx| Buffer::local("stale", cx));
        let files = [&first, &stale]
            .into_iter()
            .map(|buffer| StagedFile {
                buffer: buffer.clone(),
                version: buffer.read(cx).version(),
                edits: vec![(0..1, "x".into())],
            })
            .collect();
        stale.update(cx, |buffer, cx| {
            buffer.set_text("modified", cx);
        });
        assert!(apply_preview(files, cx).is_err());
        assert_eq!(first.read(cx).text(), "first");
        assert_eq!(stale.read(cx).text(), "modified");
    }

    #[gpui::test]
    fn test_preview_utf16_ranges_and_transaction(cx: &mut App) {
        let first = cx.new(|cx| Buffer::local("a🧪b", cx));
        let second = cx.new(|cx| Buffer::local("index", cx));
        let edit = |start, end| lsp::TextEdit {
            range: lsp::Range::new(lsp::Position::new(0, start), lsp::Position::new(0, end)),
            new_text: "x".into(),
        };
        assert!(edit_offsets(first.read(cx), vec![edit(1, 2)]).is_err());
        let files = [(&first, edit(1, 3)), (&second, edit(0, 5))]
            .into_iter()
            .map(|(buffer, edit)| StagedFile {
                buffer: buffer.clone(),
                version: buffer.read(cx).version(),
                edits: edit_offsets(buffer.read(cx), vec![edit]).expect("valid edit"),
            })
            .collect();
        let transaction = apply_preview(files, cx).expect("apply preview");
        assert_eq!(transaction.0.len(), 2);
        assert_eq!(first.read(cx).text(), "axb");
        assert_eq!(second.read(cx).text(), "x");
        for (buffer, original) in [(first, "a🧪b"), (second, "index")] {
            buffer.update(cx, |buffer, cx| {
                assert!(buffer.undo(cx).is_some());
                assert_eq!(buffer.text(), original);
            });
        }
    }

    #[test]
    fn test_interactive_validation_and_capability_gate() {
        assert!(!interactive_refactor::supports_resolution(
            &lsp::ServerCapabilities::default()
        ));
        let mut field = Field {
            id: "value".into(),
            description: "Value".into(),
            kind: FieldKind::Number,
            required: true,
            default: Value::Null,
            error: None,
        };
        assert_eq!(
            parse_answer(&field, "7".into()).expect("valid answer"),
            json!(7)
        );
        assert!(parse_answer(&field, "bad".into()).is_err());
        assert!(parse_answer(&field, String::new()).is_err());
        field.required = false;
        assert_eq!(
            parse_answer(&field, String::new()).expect("optional answer"),
            Value::Null
        );
    }
}
