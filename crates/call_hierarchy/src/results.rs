use std::collections::HashMap;

use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollStrategy, Task,
    UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use language::{Anchor, Buffer, Location, ToPoint, ToPointUtf16};
use project::{LanguageServerToQuery, lsp_command::PrepareCallHierarchy};
use ui::{Disclosure, KeyBinding, ListItem, Tooltip, prelude::*};
use workspace::{Item, SplitDirection, Workspace, item::ItemEvent};

use super::{
    Call, HierarchyItem, HierarchyMode, PickerFrame, ShowIncomingCallsInResults,
    ShowOutgoingCallsInResults, ShowSubtypesInResults, ShowSupertypesInResults, ToggleDirection,
    call_display_path, compute_call_display, editor_hierarchy_position, fetch_calls,
    prepare_hierarchy, render_item,
};

pub(super) fn register_editor_actions(editor: &mut Editor, cx: &Context<Editor>) {
    register::<ShowIncomingCallsInResults>(editor, HierarchyMode::Incoming, cx);
    register::<ShowOutgoingCallsInResults>(editor, HierarchyMode::Outgoing, cx);
    register::<ShowSupertypesInResults>(
        editor,
        HierarchyMode::Types(project::TypeHierarchyDirection::Supertypes),
        cx,
    );
    register::<ShowSubtypesInResults>(
        editor,
        HierarchyMode::Types(project::TypeHierarchyDirection::Subtypes),
        cx,
    );
}

fn register<A: Action>(editor: &mut Editor, mode: HierarchyMode, cx: &Context<Editor>) {
    let editor_handle = cx.weak_entity();
    editor
        .register_action(move |_: &A, window, cx| {
            let Ok((workspace, origin)) = editor_handle.read_with(cx, |editor, cx| {
                (editor.workspace(), editor_hierarchy_position(editor, cx))
            }) else {
                return;
            };
            if let Some(workspace) = workspace {
                workspace.update(cx, |workspace, cx| {
                    let results = cx.new(|cx| {
                        let mut results = HierarchyResults::new(workspace, mode, cx);
                        results.origin = origin;
                        results.prepare(window, cx);
                        results
                    });
                    workspace.add_item_to_active_pane(Box::new(results), None, true, window, cx);
                });
            }
        })
        .detach();
}

async fn prepare_results(
    project: &Entity<project::Project>,
    mode: HierarchyMode,
    buffer: &Entity<Buffer>,
    position: Anchor,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<Option<HierarchyItem>> {
    if mode.is_type_hierarchy() || !project.read_with(cx, |project, _| project.is_local()) {
        return prepare_hierarchy(project, mode, buffer, position, cx).await;
    }

    // Use the first capable local server so a failed prepare request can be shown in the tab.
    project
        .update(cx, |project, cx| {
            let position = position.to_point_utf16(buffer.read(cx));
            project.request_lsp(
                buffer.clone(),
                LanguageServerToQuery::FirstCapable,
                PrepareCallHierarchy { position },
                cx,
            )
        })
        .await
        .map(|items| items.into_iter().next().map(HierarchyItem::Call))
}

pub(super) struct TransferredHierarchy {
    pub root: HierarchyItem,
    pub mode: HierarchyMode,
    pub trail: Vec<PickerFrame>,
    pub calls: Vec<Call>,
    pub selected: Option<usize>,
}

pub(super) fn open_transferred(
    workspace: &mut Workspace,
    seed: TransferredHierarchy,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let results = cx.new(|cx| {
        let mut results = HierarchyResults::new(workspace, seed.mode, cx);
        let tree = Tree::from_picker(seed, cx);
        results
            .scroll
            .scroll_to_item(tree.selected, ScrollStrategy::Nearest);
        results.trees.insert(results.mode, tree);
        results
    });
    workspace.add_item_to_active_pane(Box::new(results), None, true, window, cx);
}

type NodeId = usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Row {
    Symbol(NodeId),
    Site(NodeId, usize),
}

impl Row {
    fn symbol(self) -> NodeId {
        match self {
            Row::Symbol(node) | Row::Site(node, _) => node,
        }
    }
}

#[derive(Clone)]
struct Node {
    call: Call,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    expanded: bool,
    recursive: bool,
    loaded: bool,
    error: Option<SharedString>,
}

impl Node {
    fn new(call: Call, parent: Option<NodeId>, recursive: bool) -> Self {
        Self {
            call,
            parent,
            children: Vec::new(),
            expanded: false,
            recursive,
            loaded: false,
            error: None,
        }
    }

    fn can_expand(&self) -> bool {
        (!self.loaded && !self.recursive)
            || !self.children.is_empty()
            || !self.call.sites.is_empty()
    }
}

#[derive(Clone)]
struct Tree {
    nodes: Vec<Node>,
    visible: Vec<(Row, usize)>,
    selected: usize,
}

impl Tree {
    fn new(root: HierarchyItem, cx: &App) -> Self {
        let mut call = Call::new(root, Vec::new());
        call.display = compute_call_display(&call, cx);
        Self {
            nodes: vec![Node::new(call, None, false)],
            visible: vec![(Row::Symbol(0), 0)],
            selected: 0,
        }
    }

    fn from_picker(seed: TransferredHierarchy, cx: &App) -> Self {
        let original_root = seed.trail.first().map_or(&seed.root, |frame| &frame.item);
        let mut tree = Self::new(original_root.clone(), cx);
        let mut node = 0;
        for frame in seed.trail {
            tree.expand_cached(node, frame.calls, cx);
            let Some(child) = tree.child(node, frame.child_index) else {
                tree.rebuild(Some(Row::Symbol(node)));
                return tree;
            };
            node = child;
            if tree.nodes.get(node).is_some_and(|node| node.recursive) {
                tree.rebuild(Some(Row::Symbol(node)));
                return tree;
            }
        }
        tree.expand_cached(node, seed.calls, cx);
        let selected = seed.selected.and_then(|index| tree.child(node, index));
        tree.rebuild(Some(Row::Symbol(selected.unwrap_or(node))));
        tree
    }

    fn expand_cached(&mut self, node: NodeId, calls: Vec<Call>, cx: &App) {
        if let Some(branch) = self.nodes.get_mut(node) {
            branch.expanded = true;
            self.set_children(node, calls, cx);
        }
    }

    fn set_children(&mut self, parent: NodeId, calls: Vec<Call>, cx: &App) {
        let mut children = Vec::with_capacity(calls.len());
        for call in calls {
            let recursive = std::iter::successors(self.nodes.get(parent), |node| {
                node.parent.and_then(|ancestor| self.nodes.get(ancestor))
            })
            .any(|node| node.call.item.same_symbol(&call.item, cx));
            children.push(self.nodes.len());
            self.nodes.push(Node::new(call, Some(parent), recursive));
        }
        if let Some(node) = self.nodes.get_mut(parent) {
            node.loaded = true;
            node.children = children;
        }
    }

    fn child(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.nodes.get(node)?.children.get(index).copied()
    }

    fn selected_row(&self) -> Option<Row> {
        self.visible.get(self.selected).map(|(row, _)| *row)
    }

    fn rebuild(&mut self, selected: Option<Row>) {
        let selected = selected.or_else(|| self.selected_row());
        self.visible.clear();
        let mut pending = vec![(Row::Symbol(0), 0)];
        while let Some((row, depth)) = pending.pop() {
            self.visible.push((row, depth));
            if let Row::Symbol(id) = row
                && let Some(node) = self.nodes.get(id).filter(|node| node.expanded)
            {
                pending.extend(
                    (0..node.call.sites.len())
                        .rev()
                        .map(|site| (Row::Site(id, site), depth + 1)),
                );
                pending.extend(
                    node.children
                        .iter()
                        .rev()
                        .map(|child| (Row::Symbol(*child), depth + 1)),
                );
            }
        }
        self.selected = selected
            .and_then(|selected| self.visible.iter().position(|(row, _)| *row == selected))
            .unwrap_or(0);
    }
}

pub(super) struct HierarchyResults {
    workspace: WeakEntity<Workspace>,
    project: Entity<project::Project>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    mode: HierarchyMode,
    trees: HashMap<HierarchyMode, Tree>,
    origin: Option<(Entity<Buffer>, Anchor)>,
    prepare_task: Option<Task<()>>,
    prepare_error: Option<SharedString>,
    pending: HashMap<(HierarchyMode, NodeId), Task<()>>,
}

impl HierarchyResults {
    fn new(workspace: &Workspace, mode: HierarchyMode, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().clone(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            mode,
            trees: HashMap::new(),
            origin: None,
            prepare_task: None,
            prepare_error: None,
            pending: HashMap::new(),
        }
    }

    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((buffer, position)) = self.origin.clone() else {
            return;
        };
        let project = self.project.clone();
        let mode = self.mode;
        self.prepare_error = None;
        self.prepare_task = Some(cx.spawn_in(window, async move |this, cx| {
            let prepared = prepare_results(&project, mode, &buffer, position, cx).await;
            this.update_in(cx, |this, window, cx| {
                this.prepare_task = None;
                match prepared {
                    Ok(Some(root)) => {
                        this.origin = None;
                        this.trees.insert(mode, Tree::new(root, cx));
                        this.expand(0, window, cx);
                        cx.emit(ItemEvent::UpdateTab);
                    }
                    Ok(None) => {}
                    Err(error) => this.prepare_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn tree(&self) -> Option<&Tree> {
        self.trees.get(&self.mode)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tree) = self
            .trees
            .get_mut(&self.mode)
            .filter(|tree| index < tree.visible.len())
        {
            tree.selected = index;
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn move_selection(&mut self, offset: isize, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree() {
            self.select(tree.selected.saturating_add_signed(offset), cx);
        }
    }

    fn expand(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode;
        let Some(tree) = self.trees.get_mut(&mode) else {
            return;
        };
        let Some(branch) = tree
            .nodes
            .get_mut(node)
            .filter(|branch| branch.can_expand())
        else {
            return;
        };
        branch.expanded = true;
        let fetch =
            !branch.loaded && !branch.recursive && !self.pending.contains_key(&(mode, node));
        let item = fetch.then(|| branch.call.item.clone());
        if fetch {
            branch.error = None;
        }
        tree.rebuild(None);
        cx.notify();

        if let Some(item) = item {
            let project = self.project.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                let response = fetch_calls(&item, &project, mode, cx).await;
                this.update_in(cx, |this, _, cx| {
                    this.pending.remove(&(mode, node));
                    if let Some(tree) = this.trees.get_mut(&mode) {
                        match response {
                            Ok(calls) => tree.set_children(node, calls, cx),
                            Err(error) => {
                                if let Some(branch) = tree.nodes.get_mut(node) {
                                    branch.error = Some(format!("{error:#}").into());
                                }
                            }
                        }
                        tree.rebuild(None);
                    }
                    cx.notify();
                })
                .ok();
            });
            self.pending.insert((mode, node), task);
        }
    }

    fn collapse(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(tree) = self.trees.get_mut(&self.mode)
            && let Some(branch) = tree.nodes.get_mut(node)
        {
            branch.expanded = false;
            tree.rebuild(Some(Row::Symbol(node)));
            cx.notify();
        }
    }

    fn select_child(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree() else { return };
        let Some(Row::Symbol(node)) = tree.selected_row() else {
            return;
        };
        let Some(branch) = tree.nodes.get(node) else {
            return;
        };
        if branch.expanded
            && branch.error.is_none()
            && let Some((_, child_depth)) = tree.visible.get(tree.selected + 1)
            && let Some((_, depth)) = tree.visible.get(tree.selected)
            && child_depth > depth
        {
            self.select(tree.selected + 1, cx);
        } else {
            self.expand(node, window, cx);
        }
    }

    fn select_parent(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = self.tree() else { return };
        let Some(row) = tree.selected_row() else {
            return;
        };
        let Some(branch) = tree.nodes.get(row.symbol()) else {
            return;
        };
        let parent = match row {
            Row::Symbol(node) if branch.expanded && branch.can_expand() => {
                self.collapse(node, cx);
                return;
            }
            Row::Symbol(_) => branch.parent,
            Row::Site(node, _) => Some(node),
        };
        if let Some(index) = tree
            .visible
            .iter()
            .position(|(row, _)| Some(*row) == parent.map(Row::Symbol))
        {
            self.select(index, cx);
        }
    }

    fn toggle_direction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.tree().and_then(|tree| tree.nodes.first()) else {
            return;
        };
        let mode = self.mode.opposite();
        let new_tree = !self.trees.contains_key(&mode);
        if new_tree {
            self.trees
                .insert(mode, Tree::new(root.call.item.clone(), cx));
        }
        self.mode = mode;
        if new_tree {
            self.expand(0, window, cx);
        }
        if let Some(tree) = self.tree() {
            self.scroll
                .scroll_to_item(tree.selected, ScrollStrategy::Nearest);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn open_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree() else { return };
        let Some(row) = tree.selected_row() else {
            return;
        };
        let Some(node) = tree.nodes.get(row.symbol()) else {
            return;
        };
        let location = match row {
            Row::Symbol(_) => node.call.item.location(),
            Row::Site(_, index) => {
                let Some(location) = node.call.sites.get(index) else {
                    return;
                };
                location.clone()
            }
        };
        let id = cx.entity_id();
        self.workspace
            .update(cx, |workspace, cx| {
                let origin = workspace
                    .pane_for_item_id(id)
                    .unwrap_or_else(|| workspace.active_pane().clone());
                let destination = workspace
                    .panes()
                    .iter()
                    .find(|pane| {
                        *pane != &origin
                            && pane
                                .read(cx)
                                .active_item()
                                .is_none_or(|item| item.downcast::<Editor>().is_some())
                    })
                    .cloned()
                    .unwrap_or_else(|| {
                        workspace.split_pane(origin, SplitDirection::Right, window, cx)
                    });
                let position = location.range.start.to_point(location.buffer.read(cx));
                let editor = workspace.open_project_item::<Editor>(
                    Some(destination),
                    location.buffer,
                    true,
                    true,
                    true,
                    false,
                    window,
                    cx,
                );
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([position..position]);
                        },
                    );
                });
            })
            .ok();
    }

    fn render_row(
        &self,
        index: usize,
        row: Row,
        depth: usize,
        cx: &Context<Self>,
    ) -> Option<ListItem> {
        let tree = self.tree()?;
        let node = tree.nodes.get(row.symbol())?;
        let is_selected = tree.selected == index;
        let row_element = ListItem::new(index)
            .inset(true)
            .indent_level(depth.min(24))
            .toggle_state(is_selected)
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.focus.focus(window, cx);
                    this.select(index, cx);
                    if event.click_count() == 2 {
                        this.open_selected(window, cx);
                    }
                }),
            );
        match row {
            Row::Symbol(id) => {
                let disclosure = if node.can_expand() {
                    Disclosure::new("disclosure", node.expanded)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.focus.focus(window, cx);
                            this.select(index, cx);
                            let branch = this.tree().and_then(|tree| tree.nodes.get(id));
                            if branch
                                .is_some_and(|branch| branch.expanded && branch.error.is_none())
                            {
                                this.collapse(id, cx);
                            } else {
                                this.expand(id, window, cx);
                            }
                        }))
                        .into_any_element()
                } else {
                    div().w_6().into_any_element()
                };
                let (name, detail) = render_item(&node.call, std::iter::empty(), cx);
                let path = location_label(&node.call.item.location(), false, cx);
                let status = if node.recursive {
                    Some("Recursive")
                } else if self.pending.contains_key(&(self.mode, id)) {
                    Some("Loading…")
                } else if node.loaded && node.children.is_empty() {
                    Some("No related symbols")
                } else {
                    None
                };
                let error = node.error.as_ref().map(|error| {
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .truncate()
                });
                Some(
                    row_element
                        .when_some(node.error.clone(), |row, error| {
                            row.tooltip(Tooltip::text(error))
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .overflow_hidden()
                                .child(disclosure)
                                .child(name)
                                .children(detail)
                                .child(
                                    Label::new(path)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                                .children(status.map(|status| {
                                    Label::new(status)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                }))
                                .children(error),
                        ),
                )
            }
            Row::Site(_, site) => {
                let location = node.call.sites.get(site)?;
                let location = location_label(location, true, cx);
                Some(
                    row_element.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(div().w_6())
                            .child(Label::new("Call site"))
                            .child(
                                Label::new(location)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    ),
                )
            }
        }
    }
}

fn location_label(location: &Location, show_column: bool, cx: &App) -> String {
    let buffer = location.buffer.read(cx);
    let point = location.range.start.to_point(buffer);
    let line = point.row + 1;
    let column = point.column + 1;
    let label = call_display_path(buffer, cx).map_or_else(
        || format!("Line {line}"),
        |path| format!("{}:{line}", path.display()),
    );
    if show_column {
        format!("{label}:{column}")
    } else {
        label
    }
}

impl Focusable for HierarchyResults {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<ItemEvent> for HierarchyResults {}

impl Item for HierarchyResults {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        let direction = self.mode.title();
        self.tree().and_then(|tree| tree.nodes.first()).map_or_else(
            || direction.into(),
            |root| format!("{direction}: {}", root.call.item.name()).into(),
        )
    }

    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        Task::ready(Some(cx.new(|cx| {
            let mut view = Self {
                workspace: self.workspace.clone(),
                project: self.project.clone(),
                focus: cx.focus_handle(),
                scroll: UniformListScrollHandle::new(),
                mode: self.mode,
                trees: self.trees.clone(),
                origin: self.origin.clone(),
                prepare_task: None,
                prepare_error: self.prepare_error.clone(),
                pending: HashMap::new(),
            };
            if self.prepare_task.is_some() {
                view.prepare(window, cx);
            }
            for &(mode, node) in self.pending.keys() {
                view.mode = mode;
                view.expand(node, window, cx);
            }
            view.mode = self.mode;
            if let Some(tree) = view.tree() {
                view.scroll
                    .scroll_to_item(tree.selected, ScrollStrategy::Nearest);
            }
            view
        })))
    }
}

impl Render for HierarchyResults {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.tab_content_text(0, cx);
        v_flex()
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus)
            .key_context("HierarchyResults")
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectFirst, _, cx| this.select(0, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectLast, _, cx| {
                if let Some(tree) = this.tree() {
                    this.select(tree.visible.len().saturating_sub(1), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &menu::SelectChild, window, cx| this.select_child(window, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectParent, _, cx| this.select_parent(cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.open_selected(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleDirection, window, cx| this.toggle_direction(window, cx)))
            .child(h_flex().p_2().gap_2().justify_between().border_b_1().border_color(cx.theme().colors().border_variant)
                .child(Label::new(title).truncate())
                .when(self.tree().is_some(), |header| header.child(
                    Button::new("switch-direction", format!("Show {}", self.mode.opposite().title()))
                        .key_binding(KeyBinding::for_action_in(&ToggleDirection, &self.focus, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_direction(window, cx))),
                )))
            .map(|view| {
                if let Some(tree) = self.tree() {
                    view.child(uniform_list(
                        "hierarchy",
                        tree.visible.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            let Some(tree) = this.tree() else { return Vec::new() };
                            tree.visible.get(range.clone()).unwrap_or_default().iter().enumerate()
                                .filter_map(|(offset, &(row, depth))| this.render_row(range.start + offset, row, depth, cx))
                                .collect()
                        }),
                    ).flex_1().track_scroll(&self.scroll))
                } else {
                    let status = if self.prepare_task.is_some() {
                        "Preparing hierarchy…".into()
                    } else if let Some(error) = &self.prepare_error {
                        error.clone()
                    } else {
                        "No hierarchy found. Place the cursor on a symbol supported by your language server.".into()
                    };
                    view.child(h_flex().p_4().gap_2().child(Label::new(status))
                        .when(self.origin.is_some() && self.prepare_task.is_none(), |row| {
                            row.child(Button::new("retry", "Retry").on_click(
                                cx.listener(|this, _, window, cx| this.prepare(window, cx)),
                            ))
                        }))
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use gpui::{TestAppContext, VisualTestContext};
    use lsp::request::{
        CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
    };

    use super::*;
    use crate::{
        CallHierarchyView, OpenHierarchyInResultsTab, ShowIncomingCalls,
        tests::{
            init_test, make_lsp_call_hierarchy_item, setup_call_hierarchy_test, setup_modal_test,
        },
    };

    fn active_results(
        workspace: &Entity<Workspace>,
        cx: &VisualTestContext,
    ) -> Entity<HierarchyResults> {
        workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<HierarchyResults>(cx)
            })
            .expect("active hierarchy results tab")
    }

    fn visible_symbols(view: &HierarchyResults) -> Vec<(&str, usize)> {
        view.tree()
            .into_iter()
            .flat_map(|tree| {
                tree.visible.iter().filter_map(|(row, depth)| match row {
                    Row::Symbol(node) => tree
                        .nodes
                        .get(*node)
                        .map(|node| (node.call.item.name(), *depth)),
                    Row::Site(_, _) => None,
                })
            })
            .collect()
    }

    #[gpui::test]
    async fn test_direct_results_navigation_cycle_and_independent_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, server, uri, cx) = setup_modal_test(
            "fn root() {}\nfn caller() { root(); root(); }\nfn callee() {}\n",
            cx,
        )
        .await;
        let root = make_lsp_call_hierarchy_item("root", uri.clone(), 0);
        let caller = make_lsp_call_hierarchy_item("caller", uri.clone(), 1);
        let callee = make_lsp_call_hierarchy_item("callee", uri, 2);

        server.set_request_handler::<CallHierarchyPrepare, _, _>({
            let root = root.clone();
            let caller = caller.clone();
            move |params, _| {
                let item = if params.text_document_position_params.position.line == 1 {
                    caller.clone()
                } else {
                    root.clone()
                };
                async move { Ok(Some(vec![item])) }
            }
        });
        let request_count = Arc::new(AtomicUsize::new(0));
        server.set_request_handler::<CallHierarchyIncomingCalls, _, _>({
            let request_count = request_count.clone();
            move |params, _| {
                request_count.fetch_add(1, Ordering::SeqCst);
                let call = if params.item.name == "root" {
                    lsp::CallHierarchyIncomingCall {
                        from: caller.clone(),
                        from_ranges: vec![
                            lsp::Range::new(lsp::Position::new(1, 14), lsp::Position::new(1, 18)),
                            lsp::Range::new(lsp::Position::new(1, 22), lsp::Position::new(1, 26)),
                        ],
                    }
                } else {
                    assert_eq!(params.item.name, "caller");
                    lsp::CallHierarchyIncomingCall {
                        from: root.clone(),
                        from_ranges: Vec::new(),
                    }
                };
                async move { Ok(Some(vec![call])) }
            }
        });
        server.set_request_handler::<CallHierarchyOutgoingCalls, _, _>(move |_, _| {
            let callee = callee.clone();
            async move {
                Ok(Some(vec![lsp::CallHierarchyOutgoingCall {
                    to: callee,
                    from_ranges: Vec::new(),
                }]))
            }
        });

        cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("j", menu::SelectNext, Some("HierarchyResults")),
                gpui::KeyBinding::new("h", menu::SelectParent, Some("HierarchyResults")),
                gpui::KeyBinding::new("l", menu::SelectChild, Some("HierarchyResults")),
                gpui::KeyBinding::new("enter", menu::Confirm, Some("HierarchyResults")),
            ]);
        });

        cx.dispatch_action(ShowIncomingCallsInResults);
        cx.executor().run_until_parked();
        let first = active_results(&workspace, cx);
        first.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "Callers: root");
            assert_eq!(visible_symbols(view), [("root", 0), ("caller", 1)]);
        });
        assert_eq!(request_count.load(Ordering::SeqCst), 1);

        cx.simulate_keystrokes("j l");
        cx.executor().run_until_parked();
        first.read_with(cx, |view, _| {
            assert_eq!(
                visible_symbols(view),
                [("root", 0), ("caller", 1), ("root", 2)]
            );
            let tree = view.tree().expect("caller tree");
            assert!(
                tree.nodes
                    .iter()
                    .any(|node| node.recursive && node.call.item.name() == "root")
            );
        });
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        cx.simulate_keystrokes("l l h h");
        cx.executor().run_until_parked();
        first.read_with(cx, |view, _| {
            assert_eq!(visible_symbols(view), [("root", 0), ("caller", 1)]);
        });
        cx.simulate_keystrokes("l");
        cx.executor().run_until_parked();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        cx.dispatch_action(ToggleDirection);
        cx.executor().run_until_parked();
        first.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "Callees: root");
            assert_eq!(visible_symbols(view), [("root", 0), ("callee", 1)]);
        });
        cx.dispatch_action(ToggleDirection);
        cx.executor().run_until_parked();
        first.read_with(cx, |view, _| {
            assert_eq!(
                visible_symbols(view),
                [("root", 0), ("caller", 1), ("root", 2)]
            );
            assert_eq!(view.tree().expect("caller tree").selected, 1);
        });

        cx.dispatch_action(menu::SelectLast);
        cx.simulate_keystrokes("enter");
        cx.executor().run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("source editor");
            let (buffer, anchor) =
                editor_hierarchy_position(editor.read(cx), cx).expect("call site");
            assert_eq!(
                anchor.to_point(buffer.read(cx)),
                language::Point::new(1, 22)
            );
            let results_pane = workspace
                .pane_for_item_id(first.entity_id())
                .expect("first results pane");
            assert_ne!(&results_pane, workspace.active_pane());
        });

        cx.dispatch_action(ShowOutgoingCallsInResults);
        cx.executor().run_until_parked();
        let second = active_results(&workspace, cx);
        assert_ne!(first, second);
        second.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "Callees: caller");
            assert_eq!(view.tree().expect("second tree").selected, 0);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&first, true, true, window, cx);
        });
        assert_eq!(active_results(&workspace, cx), first);
        first.read_with(cx, |view, _| {
            let tree = view.tree().expect("first tree");
            assert_eq!(view.mode, HierarchyMode::Incoming);
            assert_eq!(tree.selected_row(), Some(Row::Site(1, 1)));
            assert!(tree.nodes.get(1).is_some_and(|node| node.expanded));
        });
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        cx.dispatch_action(workspace::pane::SplitRight::default());
        cx.executor().run_until_parked();
        let split = active_results(&workspace, cx);
        assert_ne!(first, split);
        split.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "Callers: root");
            assert_eq!(
                view.tree().and_then(Tree::selected_row),
                Some(Row::Site(1, 1))
            );
        });
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn test_transfer_reuses_deep_picker_results(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, server, uri, cx) = setup_modal_test(
            "fn root() {}\nfn first() {}\nfn sibling() {}\nfn second() {}\nfn leaf() {}\n",
            cx,
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let prepares = Arc::new(AtomicUsize::new(0));
        server.set_request_handler::<CallHierarchyPrepare, _, _>({
            let uri = uri.clone();
            let prepares = prepares.clone();
            move |_, _| {
                prepares.fetch_add(1, Ordering::SeqCst);
                let root = make_lsp_call_hierarchy_item("root", uri.clone(), 0);
                async move { Ok(Some(vec![root])) }
            }
        });
        server.set_request_handler::<CallHierarchyIncomingCalls, _, _>({
            let calls = calls.clone();
            move |params, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                let children: &[(&str, u32)] = match params.item.name.as_str() {
                    "root" => &[("first", 1), ("sibling", 2)],
                    "first" => &[("second", 3)],
                    "second" => &[("leaf", 4)],
                    "leaf" => &[],
                    _ => panic!("unexpected call hierarchy request"),
                };
                let related = children
                    .iter()
                    .map(|(name, line)| lsp::CallHierarchyIncomingCall {
                        from: make_lsp_call_hierarchy_item(name, uri.clone(), *line),
                        from_ranges: Vec::new(),
                    })
                    .collect();
                async move { Ok(Some(related)) }
            }
        });
        cx.update(|_, cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-shift-enter",
                OpenHierarchyInResultsTab,
                Some("CallHierarchyPicker > Picker > Editor"),
            )]);
        });

        cx.dispatch_action(ShowIncomingCalls);
        cx.executor().run_until_parked();
        for _ in 0..2 {
            cx.dispatch_action(menu::SelectChild);
            cx.executor().run_until_parked();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        cx.simulate_keystrokes("ctrl-shift-enter");
        cx.executor().run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<CallHierarchyView>(cx).is_none()
        }));

        let results = active_results(&workspace, cx);
        results.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "Callers: root");
            assert_eq!(
                visible_symbols(view),
                [
                    ("root", 0),
                    ("first", 1),
                    ("second", 2),
                    ("leaf", 3),
                    ("sibling", 1),
                ]
            );
            let tree = view.tree().expect("transferred tree");
            let selected = tree.selected_row().expect("transferred selection");
            let node = tree.nodes.get(selected.symbol()).expect("selected symbol");
            assert_eq!(node.call.item.name(), "leaf");
        });
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        cx.dispatch_action(menu::SelectChild);
        cx.executor().run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        results.read_with(cx, |view, _| {
            let tree = view.tree().expect("transferred tree");
            let node = tree
                .selected_row()
                .and_then(|row| tree.nodes.get(row.symbol()))
                .expect("leaf");
            assert!(node.loaded && node.children.is_empty());
        });
    }

    #[gpui::test]
    async fn test_results_empty_and_request_errors(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, server, uri, cx) = setup_modal_test("fn root() {}\n", cx).await;
        server.set_request_handler::<CallHierarchyPrepare, _, _>(|_, _| async move { Ok(None) });
        cx.dispatch_action(ShowIncomingCallsInResults);
        cx.executor().run_until_parked();
        let results = active_results(&workspace, cx);
        results.read_with(cx, |view, _| {
            assert!(
                view.tree().is_none()
                    && view.prepare_error.is_none()
                    && view.prepare_task.is_none()
            );
        });

        server.set_request_handler::<CallHierarchyPrepare, _, _>(|_, _| async move {
            anyhow::bail!("prepare failed")
        });
        results.update_in(cx, |view, window, cx| view.prepare(window, cx));
        cx.executor().run_until_parked();
        results.read_with(cx, |view, _| {
            assert!(
                view.prepare_error
                    .as_ref()
                    .is_some_and(|error| error.contains("prepare failed"))
            );
        });

        server.set_request_handler::<CallHierarchyPrepare, _, _>(move |_, _| {
            let root = make_lsp_call_hierarchy_item("root", uri.clone(), 0);
            async move { Ok(Some(vec![root])) }
        });
        server.set_request_handler::<CallHierarchyIncomingCalls, _, _>(|_, _| async move {
            anyhow::bail!("cannot find callers")
        });
        results.update_in(cx, |view, window, cx| view.prepare(window, cx));
        cx.executor().run_until_parked();
        results.read_with(cx, |view, _| {
            let root = view
                .tree()
                .and_then(|tree| tree.nodes.first())
                .expect("root node");
            assert!(
                root.error
                    .as_ref()
                    .is_some_and(|error| error.contains("cannot find callers"))
            );
        });

        server.set_request_handler::<CallHierarchyIncomingCalls, _, _>(
            |_, _| async move { Ok(None) },
        );
        cx.dispatch_action(menu::SelectChild);
        cx.executor().run_until_parked();
        results.read_with(cx, |view, _| {
            let root = view
                .tree()
                .and_then(|tree| tree.nodes.first())
                .expect("retried root");
            assert!(root.error.is_none() && root.loaded && root.children.is_empty());
        });
    }

    #[gpui::test]
    async fn test_pending_branch_finishes_in_its_original_direction(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, server, uri, cx) =
            setup_modal_test("fn root() {}\nfn caller() {}\nfn ancestor() {}\n", cx).await;
        server.set_request_handler::<CallHierarchyPrepare, _, _>({
            let uri = uri.clone();
            move |_, _| {
                let root = make_lsp_call_hierarchy_item("root", uri.clone(), 0);
                async move { Ok(Some(vec![root])) }
            }
        });
        let (release_branch, wait_for_branch) = futures::channel::oneshot::channel();
        let mut wait_for_branch = Some(wait_for_branch);
        server.set_request_handler::<CallHierarchyIncomingCalls, _, _>(move |params, _| {
            let (name, line, wait) = if params.item.name == "root" {
                ("caller", 1, None)
            } else {
                assert_eq!(params.item.name, "caller");
                (
                    "ancestor",
                    2,
                    Some(wait_for_branch.take().expect("only one branch request")),
                )
            };
            let from = make_lsp_call_hierarchy_item(name, uri.clone(), line);
            async move {
                if let Some(wait) = wait {
                    wait.await?;
                }
                Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                    from,
                    from_ranges: Vec::new(),
                }]))
            }
        });
        server.set_request_handler::<CallHierarchyOutgoingCalls, _, _>(
            |_, _| async move { Ok(None) },
        );

        cx.dispatch_action(ShowIncomingCallsInResults);
        cx.executor().run_until_parked();
        let results = active_results(&workspace, cx);
        cx.dispatch_action(menu::SelectNext);
        cx.dispatch_action(menu::SelectChild);
        cx.executor().run_until_parked();
        assert!(results.read_with(cx, |view, _| {
            view.pending.contains_key(&(HierarchyMode::Incoming, 1))
        }));
        cx.dispatch_action(menu::SelectParent);
        cx.dispatch_action(ToggleDirection);
        cx.executor().run_until_parked();

        release_branch.send(()).expect("release incoming request");
        cx.executor().run_until_parked();
        results.read_with(cx, |view, _| {
            assert_eq!(view.mode, HierarchyMode::Outgoing);
            assert_eq!(visible_symbols(view), [("root", 0)]);
            assert!(view.pending.is_empty());
        });

        cx.dispatch_action(ToggleDirection);
        cx.executor().run_until_parked();
        assert_eq!(
            results.read_with(cx, |view, _| visible_symbols(view).len()),
            2
        );
        cx.dispatch_action(menu::SelectChild);
        cx.executor().run_until_parked();
        results.read_with(cx, |view, _| {
            assert_eq!(
                visible_symbols(view),
                [("root", 0), ("caller", 1), ("ancestor", 2)]
            );
        });
    }

    #[gpui::test]
    async fn test_results_unsupported_language_server(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, server, _, cx) =
            setup_call_hierarchy_test("fn root() {}\n", false, cx).await;
        server.set_request_handler::<CallHierarchyPrepare, _, _>(|_, _| async move {
            panic!("unsupported server received prepare request")
        });
        cx.dispatch_action(ShowIncomingCallsInResults);
        cx.executor().run_until_parked();
        active_results(&workspace, cx).read_with(cx, |view, _| {
            assert!(
                view.tree().is_none()
                    && view.prepare_error.is_none()
                    && view.prepare_task.is_none()
            );
        });
    }
}
