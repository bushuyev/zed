use std::collections::{HashMap, HashSet};

use futures::channel::oneshot;
use gpui::DismissEvent;
use lsp::{
    Edit, LanguageServer,
    interactive_refactor::{self, Answer, Field, FieldKind, InteractiveCommand, PreviewEdit},
};
use serde_json::Value;
use ui::Divider;

use super::*;
use workspace::{
    DismissDecision, Item, ModalView, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    item::ItemEvent,
};

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
    let signature_form = original_command.command == interactive_refactor::CHANGE_SIGNATURE;
    let force_available = interactive_refactor::supports_force_preview(
        &server.capabilities(),
        &original_command.command,
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
        command.force = false;
        for field in std::mem::take(&mut command.form_fields) {
            anyhow::ensure!(
                field_ids.insert(field.id.clone()),
                "The refactoring form contains duplicate field IDs"
            );
            let current = answers
                .get(&field.id)
                .cloned()
                .unwrap_or_else(|| field.default.clone());
            let Some(submission) = show_form(
                workspace,
                &title,
                field,
                current,
                signature_form,
                force_available,
                cx,
            )
            .await?
            else {
                return Ok(());
            };
            command.force |= submission.force;
            let answer = submission.answer;
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
    let confirmed = show_preview(workspace, &title, &staged, cx).await?;
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
    multibuffer: Entity<MultiBuffer>,
    warnings: Vec<String>,
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
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
        multibuffer.set_all_diff_hunks_expanded(cx);
        multibuffer
    });
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
            add_preview_diff(&multibuffer, &buffer, original, modified, cx).await;
            files.push(StagedFile {
                buffer,
                version,
                edits,
            });
        }
    }
    Ok(StagedPreview {
        files,
        multibuffer,
        warnings,
    })
}

async fn add_preview_diff(
    multibuffer: &Entity<MultiBuffer>,
    source: &Entity<Buffer>,
    original: String,
    modified: String,
    cx: &mut AsyncWindowContext,
) {
    let (path, file, language) = source.read_with(cx, |source_buffer, cx| {
        (
            PathKey::for_buffer(source, cx),
            source_buffer.file().cloned(),
            source_buffer.language().cloned(),
        )
    });
    let base = cx.new(|cx| {
        let mut buffer = Buffer::local(original, cx);
        buffer.set_capability(Capability::ReadOnly, cx);
        buffer.set_language_async(language.clone(), cx);
        buffer
    });
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::build(
            text::Buffer::new(
                ReplicaId::LOCAL,
                cx.entity_id().as_non_zero_u64().into(),
                modified,
            ),
            file,
            Capability::ReadOnly,
            cx,
        );
        buffer.set_language_async(language, cx);
        buffer
    });
    let base_snapshot = base.read_with(cx, |buffer, _| buffer.snapshot());
    let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
    let diff =
        cx.new(|cx| buffer_diff::BufferDiff::new_with_base_text_buffer(&buffer_snapshot, base, cx));
    let update = diff.read_with(cx, |diff, cx| {
        diff.update_diff(
            buffer_snapshot,
            &base_snapshot,
            Some(base_snapshot.text().into()),
            cx,
        )
    });
    let update = update.await;
    let ranges = diff.update(cx, |diff, cx| {
        diff.set_snapshot(update, cx);
        let snapshot = buffer.read(cx).snapshot();
        diff.snapshot(cx)
            .hunks(&snapshot)
            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>()
    });
    multibuffer.update(cx, |multibuffer, cx| {
        multibuffer.set_excerpts_for_path(path, buffer, ranges, multibuffer_context_lines(cx), cx);
        multibuffer.add_diff(diff, cx);
    });
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

struct FormSubmission {
    answer: Answer,
    force: bool,
}

async fn show_form(
    workspace: &WeakEntity<Workspace>,
    title: &str,
    field: Field,
    initial_value: Value,
    signature_form: bool,
    force_available: bool,
    cx: &mut AsyncWindowContext,
) -> Result<Option<FormSubmission>> {
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
                    help: signature_form.then(|| {
                        cx.new(|cx| {
                            let mut editor = Editor::multi_line(window, cx);
                            editor.set_use_modal_editing(false);
                            editor.set_soft_wrap_mode(
                                language::language_settings::SoftWrap::EditorWidth,
                                cx,
                            );
                            editor.set_text(SIGNATURE_EXAMPLE, window, cx);
                            editor.move_to_beginning(&MoveToBeginning, window, cx);
                            editor.set_read_only(true);
                            editor
                        })
                    }),
                    force_available,
                    response: Some(response),
                    error: None,
                }
            });
        })
    })??;
    Ok(answer.await.ok())
}

const SIGNATURE_EXAMPLE: &str = r#"{
  "name": "calculate",
  "parameters": [
    { "oldIndex": 1, "name": "second", "type": "int" },
    { "oldIndex": 0, "name": "first", "type": "int" },
    { "oldIndex": null, "name": "new_param", "type": "int", "argument": "0" }
  ],
  "returnType": "void"
}"#;

struct RefactorForm {
    title: SharedString,
    field: Field,
    input: Entity<Editor>,
    help: Option<Entity<Editor>>,
    force_available: bool,
    response: Option<oneshot::Sender<FormSubmission>>,
    error: Option<SharedString>,
}

impl RefactorForm {
    fn submit(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.submit_with_force(false, cx);
    }

    fn submit_with_force(&mut self, force: bool, cx: &mut Context<Self>) {
        match parse_answer(&self.field, self.input.read(cx).text(cx)) {
            Ok(value) => {
                if let Some(response) = self.response.take() {
                    // The editor task may have already been closed.
                    response
                        .send(FormSubmission {
                            answer: Answer {
                                id: self.field.id.clone(),
                                value,
                            },
                            force,
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_height = if self.help.is_some() {
            (window.viewport_size().height * 0.3).min(rems(16.).to_pixels(window.rem_size()))
        } else {
            rems(17.).to_pixels(window.rem_size())
        };
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
            .child(div().h(input_height).child(self.input.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .when(self.force_available, |buttons| {
                        buttons.child(Button::new("force", "Force")
                            .tooltip(Tooltip::text("Preview even if the proposed signature fails to compile"))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_with_force(true, cx))))
                    })
                    .child(Button::new("continue", "Continue").on_click(
                        cx.listener(|this, _, window, cx| this.submit(&menu::Confirm, window, cx)),
                    )),
            )
            .when_some(self.help.clone(), |form, help| {
                form.child(Divider::horizontal())
                    .child(Label::new("Signature JSON").size(LabelSize::Small))
                    .child(Label::new(
                        "Keep the function name. Move or remove entries to reorder or delete parameters. Keep the zero-based oldIndex of each existing parameter. Use null and an argument expression for a new parameter. In Rust, use a null returnType to omit it."
                    ).size(LabelSize::Small).color(Color::Muted))
                    .child(div().h(input_height).child(help))
            })
    }
}

async fn show_preview(
    workspace: &WeakEntity<Workspace>,
    title: &str,
    staged: &StagedPreview,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let (response, confirm) = oneshot::channel();
    let title = format!("{title} Preview").into();
    workspace.update_in(cx, |workspace, window, cx| {
        let preview = cx.new(|cx| {
            let editor = cx.new(|cx| {
                let mut editor =
                    Editor::for_multibuffer(staged.multibuffer.clone(), None, window, cx);
                editor.set_use_modal_editing(false);
                editor.set_read_only(true);
                editor
            });
            RefactorPreview {
                editor,
                title,
                file_count: staged.files.len(),
                warnings: staged
                    .warnings
                    .iter()
                    .cloned()
                    .map(SharedString::from)
                    .collect(),
                response: Some(response),
            }
        });
        workspace.add_item_to_active_pane(Box::new(preview), None, true, window, cx);
    })?;
    Ok(confirm.await.is_ok())
}

struct RefactorPreview {
    editor: Entity<Editor>,
    title: SharedString,
    file_count: usize,
    warnings: Vec<SharedString>,
    response: Option<oneshot::Sender<()>>,
}

impl RefactorPreview {
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(response) = self.response.take() {
            response.send(()).ok();
        }
        cx.emit(ItemEvent::CloseItem);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.response.take();
        cx.emit(ItemEvent::CloseItem);
    }
}

impl Focusable for RefactorPreview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for RefactorPreview {}

impl Item for RefactorPreview {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }

    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, _: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        let text = if self.file_count == 1 {
            "1 file".into()
        } else {
            format!("{} files", self.file_count).into()
        };
        Some((
            vec![HighlightedText {
                text,
                highlights: Vec::new(),
            }],
            None,
        ))
    }
}

impl Render for RefactorPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context("RefactorPreview")
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel(cx)))
            .when(!self.warnings.is_empty(), |view| {
                view.child(
                    v_flex()
                        .id("refactor-preview-warnings")
                        .max_h(rems(8.))
                        .overflow_y_scroll()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .gap_1()
                        .children(self.warnings.iter().cloned().map(|warning| {
                            Label::new(warning)
                                .size(LabelSize::Small)
                                .color(Color::Warning)
                        })),
                )
            })
            .child(
                div()
                    .size_full()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.editor.clone()),
            )
    }
}

#[derive(Default)]
pub struct RefactorPreviewToolbar {
    preview: Option<WeakEntity<RefactorPreview>>,
}

impl EventEmitter<ToolbarItemEvent> for RefactorPreviewToolbar {}

impl ToolbarItemView for RefactorPreviewToolbar {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.preview = item
            .and_then(|item| item.act_as::<RefactorPreview>(cx))
            .map(|preview| preview.downgrade());
        if self.preview.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for RefactorPreviewToolbar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let preview = self.preview.clone();
        h_flex().gap_1p5().when_some(preview, |toolbar, preview| {
            toolbar
                .child(Button::new("cancel", "Cancel").on_click({
                    let preview = preview.clone();
                    move |_, _, cx| {
                        preview.update(cx, RefactorPreview::cancel).ok();
                    }
                }))
                .child(Button::new("apply", "Apply").on_click(move |_, _, cx| {
                    preview.update(cx, RefactorPreview::confirm).ok();
                }))
        })
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
                    "forcePreviewCommands": [interactive_refactor::CHANGE_SIGNATURE],
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
                    if !command.force
                        || command
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
                    assert!(command.force);
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
        test.cx.cx.dispatch_action(ChangeSignature);
        test.cx.executor().run_until_parked();
        assert!(test.update_workspace(|workspace, _, cx| {
            workspace.active_modal::<RefactorForm>(cx).is_some()
        }));
        test.cx.cx.dispatch_action(menu::Cancel);
        test.cx.executor().run_until_parked();
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), before);

        test.cx.cx.dispatch_action(ChangeSignature);
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
            assert!(form.force_available);
            let help = form.help.as_ref().expect("JSON example").read(cx);
            assert!(help.read_only(cx));
            assert!(help.text(cx).contains("\"argument\": \"0\""));
            form.input
                .update(cx, |input, cx| input.set_text("reorder", window, cx));
            form.submit_with_force(true, cx);
        });
        test.cx.executor().run_until_parked();
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), before);
        let preview = test
            .update_workspace(|workspace, _, cx| workspace.item_of_type::<RefactorPreview>(cx))
            .expect("signature preview");
        preview.read_with(&test.cx.cx, |preview, cx| {
            let diff = preview.editor.read(cx).text(cx);
            assert!(diff.contains("calculate(3, 10)"));
            assert!(preview.editor.read(cx).read_only(cx));
        });
        preview.update(&mut test.cx.cx, |preview, cx| preview.confirm(cx));
        test.cx.executor().run_until_parked();
        assert_eq!(editor.read_with(&test.cx.cx, Editor::text), after);
        assert!(test.update_workspace(|workspace, _, cx| {
            workspace.item_of_type::<RefactorPreview>(cx).is_none()
        }));
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
