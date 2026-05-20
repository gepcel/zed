//! REPL operations on an [`Editor`].

use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{Editor, MultiBufferOffset};
use gpui::{App, Entity, WeakEntity, Window, prelude::*};
use language::{BufferSnapshot, Language, LanguageName, Point, ToOffset};
use project::{ProjectItem as _, WorktreeId};
use workspace::{Workspace, notifications::NotificationId};

use crate::kernels::PythonEnvKernelSpecification;
use crate::repl_store::ReplStore;
use crate::session::SessionEvent;
use crate::{
    ClearCurrentOutput, ClearOutputs, Interrupt, JupyterSettings, KernelSpecification, Restart,
    Session, Shutdown,
};

pub fn assign_kernelspec(
    kernel_specification: KernelSpecification,
    weak_editor: WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let store = ReplStore::global(cx);
    if !store.read(cx).is_enabled() {
        return Ok(());
    }

    let worktree_id = crate::repl_editor::worktree_id_for_editor(weak_editor.clone(), cx)
        .context("editor is not in a worktree")?;

    store.update(cx, |store, cx| {
        store.set_active_kernelspec(worktree_id, kernel_specification.clone(), cx);
    });

    let fs = store.read(cx).fs().clone();

    if let Some(session) = store.read(cx).get_session(weak_editor.entity_id()).cloned() {
        // Drop previous session, start new one
        session.update(cx, |session, cx| {
            session.clear_outputs(cx);
            session.shutdown(window, cx);
            cx.notify();
        });
    }

    let session =
        cx.new(|cx| Session::new(weak_editor.clone(), fs, kernel_specification, window, cx));

    weak_editor
        .update(cx, |_editor, cx| {
            cx.notify();

            cx.subscribe(&session, {
                let store = store.clone();
                move |_this, _session, event, cx| match event {
                    SessionEvent::Shutdown(shutdown_event) => {
                        store.update(cx, |store, _cx| {
                            store.remove_session(shutdown_event.entity_id());
                        });
                    }
                }
            })
            .detach();
        })
        .ok();

    store.update(cx, |store, _cx| {
        store.insert_session(weak_editor.entity_id(), session.clone());
    });

    Ok(())
}

pub enum ReplRunMode {
    // A smart run action, to auto detect block
    // From [#49636](https://github.com/zed-industries/zed/pull/49636/changes#diff-4e8a38f6ee486b3ceb9c7a866f565ae73a80e22d9f070bb3d9a6c3b92db4ca31)
    Block,
    // Run a single line
    Line,
    // Run a single cell (codes between a # %% mark in python)
    Cell,
    // Run all lines above
    Above,
    // Run all cells above
    AboveCells,
    // Run all codes
    All,
}

pub fn install_ipykernel_and_assign(
    kernel_specification: KernelSpecification,
    weak_editor: WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let KernelSpecification::PythonEnv(ref env_spec) = kernel_specification else {
        return assign_kernelspec(kernel_specification, weak_editor, window, cx);
    };

    let python_path = env_spec.path.clone();
    let env_name = env_spec.name.clone();
    let is_uv = env_spec.is_uv();
    let env_spec = env_spec.clone();

    struct IpykernelInstall;
    let notification_id = NotificationId::unique::<IpykernelInstall>();

    let workspace = Workspace::for_window(window, cx);
    if let Some(workspace) = &workspace {
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                workspace::Toast::new(
                    notification_id.clone(),
                    format!("Installing ipykernel in {}...", env_name),
                ),
                cx,
            );
        });
    }

    let weak_workspace = workspace.map(|w| w.downgrade());
    let window_handle = window.window_handle();

    let install_task = cx.background_spawn(async move {
        let output = if is_uv {
            util::command::new_command("uv")
                .args(&[
                    "pip",
                    "install",
                    "ipykernel",
                    "--python",
                    &python_path.to_string_lossy(),
                ])
                .output()
                .await
                .context("failed to run uv pip install ipykernel")?
        } else {
            util::command::new_command(python_path.to_string_lossy().as_ref())
                .args(&["-m", "pip", "install", "ipykernel"])
                .output()
                .await
                .context("failed to run pip install ipykernel")?
        };

        if output.status.success() {
            anyhow::Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("{}", stderr.lines().last().unwrap_or("unknown error"))
        }
    });

    cx.spawn(async move |cx| {
        let result = install_task.await;

        match result {
            Ok(()) => {
                if let Some(weak_workspace) = &weak_workspace {
                    weak_workspace
                        .update(cx, |workspace, cx| {
                            workspace.dismiss_toast(&notification_id, cx);
                            workspace.show_toast(
                                workspace::Toast::new(
                                    notification_id.clone(),
                                    format!("ipykernel installed in {}", env_name),
                                )
                                .autohide(),
                                cx,
                            );
                        })
                        .ok();
                }

                window_handle
                    .update(cx, |_, window, cx| {
                        let store = ReplStore::global(cx);
                        store.update(cx, |store, cx| {
                            store.mark_ipykernel_installed(cx, &env_spec);
                        });

                        let updated_spec =
                            KernelSpecification::PythonEnv(PythonEnvKernelSpecification {
                                has_ipykernel: true,
                                ..env_spec
                            });
                        assign_kernelspec(updated_spec, weak_editor, window, cx).ok();
                    })
                    .ok();
            }
            Err(error) => {
                if let Some(weak_workspace) = &weak_workspace {
                    weak_workspace
                        .update(cx, |workspace, cx| {
                            workspace.dismiss_toast(&notification_id, cx);
                            workspace.show_toast(
                                workspace::Toast::new(
                                    notification_id.clone(),
                                    format!(
                                        "Failed to install ipykernel in {}: {}",
                                        env_name, error
                                    ),
                                ),
                                cx,
                            );
                        })
                        .ok();
                }
            }
        }
    })
    .detach();

    Ok(())
}

pub fn run(
    editor: WeakEntity<Editor>,
    move_down: bool,
    window: &mut Window,
    cx: &mut App,
    repl_run_mode: ReplRunMode,
) -> Result<()> {
    let store = ReplStore::global(cx);
    if !store.read(cx).is_enabled() {
        return Ok(());
    }
    store.update(cx, |store, cx| store.ensure_kernelspecs(cx));

    let editor = editor.upgrade().context("editor was dropped")?;
    let selected_range = editor
        .update(cx, |editor, cx| {
            editor
                .selections
                .newest_adjusted(&editor.display_snapshot(cx))
        })
        .range();
    let multibuffer = editor.read(cx).buffer().clone();
    let Some(buffer) = multibuffer.read(cx).as_singleton() else {
        return Ok(());
    };

    let Some(project_path) = buffer.read(cx).project_path(cx) else {
        return Ok(());
    };

    let (runnable_ranges, next_cell_point) =
        runnable_ranges(&buffer.read(cx).snapshot(), selected_range, cx, repl_run_mode);

    for runnable_range in runnable_ranges {
        let Some(language) = multibuffer.read(cx).language_at(runnable_range.start, cx) else {
            continue;
        };

        let kernel_specification = store
            .read(cx)
            .active_kernelspec(project_path.worktree_id, Some(language.clone()), cx)
            .with_context(|| format!("No kernel found for language: {}", language.name()))?;

        let fs = store.read(cx).fs().clone();

        let session = if let Some(session) = store.read(cx).get_session(editor.entity_id()).cloned()
        {
            session
        } else {
            let weak_editor = editor.downgrade();
            let session =
                cx.new(|cx| Session::new(weak_editor, fs, kernel_specification, window, cx));

            editor.update(cx, |_editor, cx| {
                cx.notify();

                cx.subscribe(&session, {
                    let store = store.clone();
                    move |_this, _session, event, cx| match event {
                        SessionEvent::Shutdown(shutdown_event) => {
                            store.update(cx, |store, _cx| {
                                store.remove_session(shutdown_event.entity_id());
                            });
                        }
                    }
                })
                .detach();
            });

            store.update(cx, |store, _cx| {
                store.insert_session(editor.entity_id(), session.clone());
            });

            session
        };

        let selected_text;
        let anchor_range;
        let next_cursor;
        {
            let snapshot = multibuffer.read(cx).read(cx);
            selected_text = snapshot
                .text_for_range(runnable_range.clone())
                .collect::<String>();
            anchor_range = snapshot.anchor_before(runnable_range.start)
                ..snapshot.anchor_after(runnable_range.end);
            next_cursor = next_cell_point.map(|point| snapshot.anchor_after(point));
        }

        session.update(cx, |session, cx| {
            session.execute(
                selected_text,
                anchor_range,
                next_cursor,
                move_down,
                window,
                cx,
            );
        });
    }

    anyhow::Ok(())
}

pub enum SessionSupport {
    ActiveSession(Entity<Session>),
    Inactive(KernelSpecification),
    RequiresSetup(LanguageName),
    Unsupported,
}

pub fn worktree_id_for_editor(editor: WeakEntity<Editor>, cx: &mut App) -> Option<WorktreeId> {
    editor.upgrade().and_then(|editor| {
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()?
            .read(cx)
            .project_path(cx)
            .map(|path| path.worktree_id)
    })
}

pub fn session(editor: WeakEntity<Editor>, cx: &mut App) -> SessionSupport {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();

    if let Some(session) = store.read(cx).get_session(entity_id).cloned() {
        return SessionSupport::ActiveSession(session);
    };

    let Some(language) = get_language(editor.clone(), cx) else {
        return SessionSupport::Unsupported;
    };

    let worktree_id = worktree_id_for_editor(editor, cx);

    let Some(worktree_id) = worktree_id else {
        return SessionSupport::Unsupported;
    };

    let kernelspec = store
        .read(cx)
        .active_kernelspec(worktree_id, Some(language.clone()), cx);

    match kernelspec {
        Some(kernelspec) => SessionSupport::Inactive(kernelspec),
        None => {
            // For language_supported, need to check available kernels for language
            if language_supported(&language, cx) {
                SessionSupport::RequiresSetup(language.name())
            } else {
                SessionSupport::Unsupported
            }
        }
    }
}

pub fn clear_outputs(editor: WeakEntity<Editor>, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };
    session.update(cx, |session, cx| {
        session.clear_outputs(cx);
        cx.notify();
    });
}

pub fn clear_current_output(editor: WeakEntity<Editor>, cx: &mut App) {
    let Some(editor_entity) = editor.upgrade() else {
        return;
    };

    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };

    let position = editor_entity.read(cx).selections.newest_anchor().head();

    session.update(cx, |session, cx| {
        session.clear_output_at_position(position, cx);
    });
}

pub fn interrupt(editor: WeakEntity<Editor>, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };

    session.update(cx, |session, cx| {
        session.interrupt(cx);
        cx.notify();
    });
}

pub fn shutdown(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };

    session.update(cx, |session, cx| {
        session.shutdown(window, cx);
        cx.notify();
    });
}

pub fn restart(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };

    let entity_id = editor.entity_id();

    let Some(session) = ReplStore::global(cx)
        .read(cx)
        .get_session(entity_id)
        .cloned()
    else {
        return;
    };

    session.update(cx, |session, cx| {
        session.restart(window, cx);
        cx.notify();
    });
}

pub fn setup_editor_session_actions(editor: &mut Editor, editor_handle: WeakEntity<Editor>) {
    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &ClearOutputs, _, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::clear_outputs(editor_handle.clone(), cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &ClearCurrentOutput, _, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::clear_current_output(editor_handle.clone(), cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &Interrupt, _, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::interrupt(editor_handle.clone(), cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &Shutdown, window, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::shutdown(editor_handle.clone(), window, cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle;
            move |_: &Restart, window, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::restart(editor_handle.clone(), window, cx);
            }
        })
        .detach();
}

fn cell_range(buffer: &BufferSnapshot, start_row: u32, end_row: u32) -> Range<Point> {
    let mut snippet_end_row = end_row;
    while buffer.is_line_blank(snippet_end_row) && snippet_end_row > start_row {
        snippet_end_row -= 1;
    }
    Point::new(start_row, 0)..Point::new(snippet_end_row, buffer.line_len(snippet_end_row))
}

// Returns the ranges of the snippets in the buffer and the next point for moving the cursor to
fn jupytext_cells(
    buffer: &BufferSnapshot,
    range: Range<Point>,
) -> (Vec<Range<Point>>, Option<Point>) {
    let mut current_row = range.start.row;

    let Some(language) = buffer.language() else {
        return (Vec::new(), None);
    };

    let default_scope = language.default_scope();
    let comment_prefixes = default_scope.line_comment_prefixes();
    if comment_prefixes.is_empty() {
        return (Vec::new(), None);
    }

    let jupytext_prefixes = comment_prefixes
        .iter()
        .map(|comment_prefix| format!("{comment_prefix}%%"))
        .collect::<Vec<_>>();

    let snippet_start_row;
    loop {
        if jupytext_prefixes
            .iter()
            .any(|prefix| buffer.contains_str_at(Point::new(current_row, 0), prefix))
        {
            snippet_start_row = Some(current_row);
            break;
        } else if current_row > 0 {
            current_row -= 1;
        } else {
            snippet_start_row = Some(0);
            break;
        }
    }

    let mut snippets = Vec::new();
    if let Some(mut snippet_start_row) = snippet_start_row {
        for current_row in range.start.row + 1..=buffer.max_point().row {
            if jupytext_prefixes
                .iter()
                .any(|prefix| buffer.contains_str_at(Point::new(current_row, 0), prefix))
            {
                snippets.push(cell_range(buffer, snippet_start_row, current_row - 1));

                if current_row <= range.end.row {
                    snippet_start_row = current_row;
                } else {
                    // Return our snippets as well as the next point for moving the cursor to
                    return (snippets, Some(Point::new(current_row, 0)));
                }
            }
        }

        // Go to the end of the buffer (no more jupytext cells found)
        snippets.push(cell_range(
            buffer,
            snippet_start_row,
            buffer.max_point().row,
        ));
    }

    (snippets, None)
}

/// Find the enclosing top-level block at the cursor position using treesitter.
/// Returns the range of the block, or the selection if non-empty.
/// From [#49636](https://github.com/zed-industries/zed/pull/49636/changes#diff-4e8a38f6ee486b3ceb9c7a866f565ae73a80e22d9f070bb3d9a6c3b92db4ca31)
fn block_range(buffer: &BufferSnapshot, selection: Range<Point>) -> Option<Range<Point>> {
    let start_offset = selection.start.to_offset(buffer);
    let end_offset = selection.end.to_offset(buffer);

    // If user has non-empty selection, use it
    if start_offset != end_offset {
        return Some(selection);
    }

    // Get syntax layer at cursor position
    let layer = buffer.syntax_layer_at(start_offset)?;
    let root_node = layer.node();
    let mut cursor = root_node.walk();

    // Descend to the deepest node containing the cursor position
    while cursor.goto_first_child_for_byte(start_offset).is_some() {}

    // Walk up until we find a node whose parent is the root
    loop {
        let node = cursor.node();
        if let Some(parent) = node.parent() {
            let parent_kind = parent.kind();
            // Common root node kinds across languages:
            // Python: module, JavaScript/TypeScript: program, Rust/Go: source_file, Lua: chunk
            if matches!(
                parent_kind,
                "module" | "program" | "source_file" | "chunk" | "translation_unit"
            ) {
                let start = buffer.offset_to_point(node.start_byte());
                let end = buffer.offset_to_point(node.end_byte());
                return Some(start..end);
            }
        }
        if !cursor.goto_parent() {
            break;
        }
    }
    None
}

// 辅助：判断行是否为空或注释行
fn is_blank_or_comment(buffer: &BufferSnapshot, row: u32) -> bool {
    if buffer.is_line_blank(row) {
        return true;
    }
    let line_range = Point::new(row, 0)..Point::new(row, buffer.line_len(row));
    let line_text: String = buffer.text_for_range(line_range).collect();
    line_text.trim().starts_with('#')
}

// 辅助：从子句节点向上查找所属父块（if/for/while/try/match）
fn find_parent_block(node: language::Node) -> Option<language::Node> {
    let mut parent = node.parent()?;
    loop {
        let kind = parent.kind();
        if matches!(
            kind,
            "if_statement" | "for_statement" | "while_statement" | "try_statement" | "match_statement"
        ) {
            return Some(parent);
        }
        parent = parent.parent()?;
    }
}

// 辅助：从语法节点构造从行首开始的范围（保留前导空格/缩进）
fn node_range_from_line_start(node: language::Node, buffer: &BufferSnapshot) -> Range<Point> {
    let start_row = node.start_position().row as u32;
    let start = Point::new(start_row, 0);
    let end = buffer.offset_to_point(node.end_byte());
    start..end
}

/// 找到当前行的【最小完整语法单元】（保留行首缩进）
/// 规则：
/// 1. 普通单行语句 → 返回当前行
/// 2. 跨行表达式 → 返回整个表达式
/// 3. 块语句（if/for/while/with/try/match 等）：
///    - 光标在块头行 → 返回整个块
///    - 光标在 elif/else/except/finally/case 行 → 返回包含它的最外层块
/// 4. 空行或注释行 → 跳转到下一个可执行行，并对其应用上述规则
fn line_runnable_range(buffer: &BufferSnapshot, range: Range<Point>) -> Range<Point> {

    let row = range.start.row;
    let line_len = buffer.line_len(row);

    // 将列限制在当前行有效范围内，防止 offset 落入下一行
    let col = if line_len == 0 {
        0
    } else {
        range.start.column.min(line_len - 1)
    };
    let offset = Point::new(row, col).to_offset(buffer);

    // The following two lines sometimes cause the offset to move to the next line when the cursor is at the end of the line, which causes the block detection to fail. So we need to ensure the offset is always within the current line.
    // let row = range.start.row;
    // let offset = range.start.to_offset(buffer);

    // 规则4：空行或注释行 → 跳转到下一个可执行行，并重新应用本函数
    if is_blank_or_comment(buffer, row) {
        let max_row = buffer.max_point().row;
        let mut next_row = row + 1;
        while next_row <= max_row && is_blank_or_comment(buffer, next_row) {
            next_row += 1;
        }
        if next_row <= max_row {
            return line_runnable_range(buffer, Point::new(next_row, 0)..Point::new(next_row, 0));
        } else {
            return Point::new(row, 0)..Point::new(row, 0);
        }
    }

    let layer = match buffer.syntax_layer_at(offset) {
        Some(layer) => layer,
        None => return Point::new(row, 0)..Point::new(row, buffer.line_len(row)),
    };
    let root = layer.node();
    let mut cursor = root.walk();

    while cursor.goto_first_child_for_byte(offset).is_some() {}
    let mut current = cursor.node();

    const SUB_CLAUSES: &[&str] = &[
        "else_clause",
        "elif_clause",
        "except_clause",
        "finally_clause",
        "case_clause",
    ];

    const BLOCK_NODES: &[&str] = &[
        "for_statement",
        "while_statement",
        "with_statement",
        "try_statement",
        "match_statement",
        "async_for_statement",
        "async_with_statement",
    ];

    let mut expr_candidate: Option<language::Node> = None;

    loop {
        let kind = current.kind();

        // 遇到 block 节点停止向上，避免进入外层函数/类
        if kind == "block" {
            break;
        }

        // if 语句：扩展到最外层 if
        if kind == "if_statement" && current.start_position().row as u32 == row {
            let mut outer = current;
            while let Some(parent) = outer.parent() {
                if parent.kind() == "if_statement" {
                    outer = parent;
                } else {
                    break;
                }
            }
            return node_range_from_line_start(outer, buffer);
        }

        // 函数/类定义：若光标在函数头（def 到冒号之间的任意行），返回整个块
        if matches!(
            kind,
            "function_definition" | "class_definition" | "async_function_definition"
        ) {
            let is_in_header = current
                .child_by_field_name("body")
                .map_or(true, |body| (row as usize) < body.start_position().row);
            if is_in_header {
                return node_range_from_line_start(current, buffer);
            }
        }

        // 特殊子句：返回所属父块
        if SUB_CLAUSES.contains(&kind) && current.start_position().row as u32 == row {
            if let Some(parent_block) = find_parent_block(current) {
                return node_range_from_line_start(parent_block, buffer);
            }
        }

        // 独立块语句（光标在头行）
        if BLOCK_NODES.contains(&kind) && current.start_position().row as u32 == row {
            return node_range_from_line_start(current, buffer);
        }

        // 记录跨行表达式候选（排除块/子句/if）
        if kind != "block"
            && !BLOCK_NODES.contains(&kind)
            && !SUB_CLAUSES.contains(&kind)
            && kind != "if_statement"
            && (current.start_position().row as u32) != (current.end_position().row as u32)
        {
            expr_candidate = Some(current);
        }

        let Some(parent) = current.parent() else {
            break;
        };
        if matches!(
            parent.kind(),
            "module" | "program" | "source_file" | "chunk" | "translation_unit"
        ) {
            break;
        }
        current = parent;
    }

    if let Some(expr) = expr_candidate {
        return node_range_from_line_start(expr, buffer);
    }

    // 普通单行：返回整行
    Point::new(row, 0)..Point::new(row, buffer.line_len(row))
}

fn runnable_ranges(
    buffer: &BufferSnapshot,
    range: Range<Point>,
    cx: &mut App,
    repl_run_mode: ReplRunMode,
) -> (Vec<Range<Point>>, Option<Point>) {
    if let Some(language) = buffer.language()
        && language.name() == "Markdown"
    {
        return (markdown_code_blocks(buffer, range, cx), None);
    }

    let snippet_range;

    match repl_run_mode {
        // Auto detect and run the code block
        ReplRunMode::Block =>{
            // Priority 1: if inside a cell, run cell
            // let (jupytext_snippets, next_cursor) = jupytext_cells(buffer, range.clone());
            // if !jupytext_snippets.is_empty() {
            //     return (jupytext_snippets, next_cursor);
            // }
            // If no cells, check if selected
            let is_empty_selection = range.start == range.end;
            //if no select
            if is_empty_selection{
                if let Some(block) = block_range(buffer, range.clone()) {
                    let start_language = buffer.language_at(block.start);
                    let end_language = buffer.language_at(block.end);

                    if start_language
                        .zip(end_language)
                        .is_some_and(|(start, end)| start == end)
                    {
                        return (vec![block], None);
                    }
                }
            }
            // if selection, return selection
            snippet_range = cell_range(buffer, range.start.row, range.end.row);
        }

        // Run a single line
        //Auto detect a single line with line breaks, and if place in
        // a for/if/def statement, should be smart enough to run the current smallest block
        ReplRunMode::Line => {
            let has_selection = range.start != range.end;
            if has_selection {
                snippet_range = cell_range(buffer, range.start.row, range.end.row);
            } else {
                snippet_range = line_runnable_range(buffer, range.clone())
            }
        }

        //Run the current cell, 
        ReplRunMode::Cell => {
            let (jupytext_snippets, next_cursor) = jupytext_cells(buffer, range.clone());
            if !jupytext_snippets.is_empty() {
                return (jupytext_snippets, next_cursor);
            } else {
                // func jupytext_cells search jupytext_prefixes backward
                // if no cell, return empty
                return (Vec::new(), None);
            }
        }

        // Run all codes above exclude the current line
        ReplRunMode::Above => {
            if range.start.row > 0 {
                snippet_range = cell_range(buffer, 0, range.start.row-1);
            } else {
                // cursor is in the first line, return empty
                return (Vec::new(), None);
            }
        }

        //Run all cells above exclude the current
        ReplRunMode::AboveCells => {
            let (jupytext_snippets, next_cursor) = jupytext_cells(buffer, Point::new(0, 0)..Point::new(range.start.row, 0));
            if !jupytext_snippets.is_empty() && jupytext_snippets.len() > 1 {
                // return all cells above exclude the current cell.
                return (jupytext_snippets[0..jupytext_snippets.len()-1].to_vec(), next_cursor);
            } else {
                // func jupytext_cells search jupytext_prefixes backward
                // if no cell, return empty
                return (Vec::new(), None);
            }
        }

        // Run all codes
        ReplRunMode::All => {
            snippet_range = cell_range(buffer, 0, buffer.max_point().row);
        }
    }

    // Check if the snippet range is entirely blank, if so, skip forward to find code
    let is_blank =
        (snippet_range.start.row..=snippet_range.end.row).all(|row| buffer.is_line_blank(row));

    if is_blank {
        // Search forward for the next non-blank line
        let max_row = buffer.max_point().row;
        let mut next_row = snippet_range.end.row + 1;
        while next_row <= max_row && buffer.is_line_blank(next_row) {
            next_row += 1;
        }

        if next_row <= max_row {
            // Found a non-blank line, find the extent of this cell
            let next_snippet_range = cell_range(buffer, next_row, next_row);
            let start_language = buffer.language_at(next_snippet_range.start);
            let end_language = buffer.language_at(next_snippet_range.end);

            if start_language
                .zip(end_language)
                .is_some_and(|(start, end)| start == end)
            {
                return (vec![next_snippet_range], None);
            }
        }

        return (Vec::new(), None);
    }

    let start_language = buffer.language_at(snippet_range.start);
    let end_language = buffer.language_at(snippet_range.end);

    if start_language
        .zip(end_language)
        .is_some_and(|(start, end)| start == end)
    {
        (vec![snippet_range], None)
    } else {
        (Vec::new(), None)
    }
}

// We allow markdown code blocks to end in a trailing newline in order to render the output
// below the final code fence. This is different than our behavior for selections and Jupytext cells.
fn markdown_code_blocks(
    buffer: &BufferSnapshot,
    range: Range<Point>,
    cx: &mut App,
) -> Vec<Range<Point>> {
    buffer
        .injections_intersecting_range(range)
        .filter(|(_, language)| language_supported(language, cx))
        .map(|(content_range, _)| {
            buffer.offset_to_point(content_range.start)..buffer.offset_to_point(content_range.end)
        })
        .collect()
}

fn language_supported(language: &Arc<Language>, cx: &mut App) -> bool {
    let store = ReplStore::global(cx);
    let store_read = store.read(cx);

    store_read
        .pure_jupyter_kernel_specifications()
        .any(|spec| language.matches_kernel_language(spec.language().as_ref()))
}

fn get_language(editor: WeakEntity<Editor>, cx: &mut App) -> Option<Arc<Language>> {
    editor
        .update(cx, |editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            let selection = editor
                .selections
                .newest::<MultiBufferOffset>(&display_snapshot);
            display_snapshot
                .buffer_snapshot()
                .language_at(selection.head())
                .cloned()
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::App;
    use indoc::indoc;
    use language::{Buffer, Language, LanguageConfig, LanguageRegistry};

    #[gpui::test]
    fn test_snippet_ranges(cx: &mut App) {
        // Create a test language
        let test_language = Arc::new(Language::new(
            LanguageConfig {
                name: "TestLang".into(),
                line_comments: vec!["# ".into()],
                ..Default::default()
            },
            None,
        ));

        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    print(1 + 1)
                    print(2 + 2)

                    print(4 + 4)


                "# },
                cx,
            )
            .with_language(test_language, cx)
        });
        let snapshot = buffer.read(cx).snapshot();

        // Single-point selection
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(0, 4)..Point::new(0, 4), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(snippets, vec!["print(1 + 1)"]);

        // Multi-line selection
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(0, 5)..Point::new(2, 0), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![indoc! { r#"
                print(1 + 1)
                print(2 + 2)"# }]
        );

        // Trimming multiple trailing blank lines
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(0, 5)..Point::new(5, 0), cx, ReplRunMode::Line);

        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![indoc! { r#"
                print(1 + 1)
                print(2 + 2)

                print(4 + 4)"# }]
        );

        // Run cell mode without jupytext_prefixes, run all codes as cell.
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(0,5)..Point::new(5,0), cx, ReplRunMode::Cell);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(snippets,
            vec![indoc! { r#"
                print(1 + 1)
                print(2 + 2)

                print(4 + 4)"# }]
        );
    }

    #[gpui::test]
    fn test_jupytext_snippet_ranges(cx: &mut App) {
        // Create a test language
        let test_language = Arc::new(Language::new(
            LanguageConfig {
                name: "TestLang".into(),
                line_comments: vec!["# ".into()],
                ..Default::default()
            },
            None,
        ));

        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    # Hello!
                    # %% [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)

                    # %%
                    print(3 + 3)
                    print(4 + 4)

                    print(5 + 5)



                "# },
                cx,
            )
            .with_language(test_language, cx)
        });
        let snapshot = buffer.read(cx).snapshot();

        // Jupytext snippet surrounding an empty selection
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(2, 5)..Point::new(2, 5), cx, ReplRunMode::Cell);

        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![indoc! { r#"
                # %% [markdown]
                # This is some arithmetic
                print(1 + 1)
                print(2 + 2)"# }]
        );

        // Jupytext snippets intersecting a non-empty selection
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(2, 5)..Point::new(6, 2), cx, ReplRunMode::Cell);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # %% [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)"#
                },
                indoc! { r#"
                    # %%
                    print(3 + 3)
                    print(4 + 4)

                    print(5 + 5)"#
                }
            ]
        );
    }

    #[gpui::test]
    fn test_markdown_code_blocks(cx: &mut App) {
        use crate::kernels::LocalKernelSpecification;
        use jupyter_protocol::JupyterKernelspec;

        // Initialize settings
        settings::init(cx);
        editor::init(cx);

        // Initialize the ReplStore with a fake filesystem
        let fs = Arc::new(project::RealFs::new(None, cx.background_executor().clone()));
        ReplStore::init(fs, cx);

        // Add mock kernel specifications for TypeScript and Python
        let store = ReplStore::global(cx);
        store.update(cx, |store, cx| {
            let typescript_spec = KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "typescript".into(),
                kernelspec: JupyterKernelspec {
                    argv: vec![],
                    display_name: "TypeScript".into(),
                    language: "typescript".into(),
                    interrupt_mode: None,
                    metadata: None,
                    env: None,
                },
                path: std::path::PathBuf::new(),
            });

            let python_spec = KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "python".into(),
                kernelspec: JupyterKernelspec {
                    argv: vec![],
                    display_name: "Python".into(),
                    language: "python".into(),
                    interrupt_mode: None,
                    metadata: None,
                    env: None,
                },
                path: std::path::PathBuf::new(),
            });

            store.set_kernel_specs_for_testing(vec![typescript_spec, python_spec], cx);
        });

        let markdown = languages::language("markdown", tree_sitter_md::LANGUAGE.into());
        let typescript = languages::language(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        );
        let python = languages::language("python", tree_sitter_python::LANGUAGE.into());
        let language_registry = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
        language_registry.add(markdown.clone());
        language_registry.add(typescript);
        language_registry.add(python);

        // Two code blocks intersecting with selection
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(
                indoc! { r#"
                    Hey this is Markdown!

                    ```typescript
                    let foo = 999;
                    console.log(foo + 1999);
                    ```

                    ```typescript
                    console.log("foo")
                    ```
                    "#
                },
                cx,
            );
            buffer.set_language_registry(language_registry.clone());
            buffer.set_language(Some(markdown.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        let (snippets, _) = runnable_ranges(&snapshot, Point::new(3, 5)..Point::new(8, 5), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    let foo = 999;
                    console.log(foo + 1999);
                    "#
                },
                "console.log(\"foo\")\n"
            ]
        );

        // Three code blocks intersecting with selection
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(
                indoc! { r#"
                    Hey this is Markdown!

                    ```typescript
                    let foo = 999;
                    console.log(foo + 1999);
                    ```

                    ```ts
                    console.log("foo")
                    ```

                    ```typescript
                    console.log("another code block")
                    ```
                "# },
                cx,
            );
            buffer.set_language_registry(language_registry.clone());
            buffer.set_language(Some(markdown.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        let (snippets, _) = runnable_ranges(&snapshot, Point::new(3, 5)..Point::new(12, 5), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    let foo = 999;
                    console.log(foo + 1999);
                    "#
                },
                "console.log(\"foo\")\n",
                "console.log(\"another code block\")\n",
            ]
        );

        // Python code block
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(
                indoc! { r#"
                    Hey this is Markdown!

                    ```python
                    print("hello there")
                    print("hello there")
                    print("hello there")
                    ```
                "# },
                cx,
            );
            buffer.set_language_registry(language_registry.clone());
            buffer.set_language(Some(markdown.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        let (snippets, _) = runnable_ranges(&snapshot, Point::new(4, 5)..Point::new(5, 5), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(
            snippets,
            vec![indoc! { r#"
                print("hello there")
                print("hello there")
                print("hello there")
                "#
            },]
        );
    }

    #[gpui::test]
    fn test_skip_blank_lines_to_next_cell(cx: &mut App) {
        let test_language = Arc::new(Language::new(
            LanguageConfig {
                name: "TestLang".into(),
                line_comments: vec!["# ".into()],
                ..Default::default()
            },
            None,
        ));

        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    print(1 + 1)

                    print(2 + 2)
                "# },
                cx,
            )
            .with_language(test_language.clone(), cx)
        });
        let snapshot = buffer.read(cx).snapshot();

        // Selection on blank line should skip to next non-blank cell
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(1, 0)..Point::new(1, 0), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(snippets, vec!["print(2 + 2)"]);

        // Multiple blank lines should also skip forward
        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    print(1 + 1)



                    print(2 + 2)
                "# },
                cx,
            )
            .with_language(test_language.clone(), cx)
        });
        let snapshot = buffer.read(cx).snapshot();

        let (snippets, _) = runnable_ranges(&snapshot, Point::new(2, 0)..Point::new(2, 0), cx, ReplRunMode::Line);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(snippets, vec!["print(2 + 2)"]);

        // Blank lines at end of file should return nothing
        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    print(1 + 1)

                "# },
                cx,
            )
            .with_language(test_language, cx)
        });
        let snapshot = buffer.read(cx).snapshot();

        let (snippets, _) = runnable_ranges(&snapshot, Point::new(1, 0)..Point::new(1, 0), cx, ReplRunMode::Line);
        assert!(snippets.is_empty());
    } 
    
    #[gpui::test]
    fn test_run_all_and_run_above_with_cells(cx: &mut App) {
        // Create a test language
        let test_language = Arc::new(Language::new(
            LanguageConfig {
                name: "TestLang".into(),
                line_comments: vec!["# ".into()],
                ..Default::default()
            },
            None,
        ));
        
        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)
                    
                    # %% [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)
    
                    # %%
                    print(3 + 3)
                    print(4 + 4)
    
                    print(5 + 5)
    
    
    
                "# },
                cx,
            )
            .with_language(test_language, cx)
        });
        let snapshot = buffer.read(cx).snapshot();
        
        // Test for repl: run all with cells
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(1, 0)..Point::new(1, 0), cx, ReplRunMode::All);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)
                   
                    # %% [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)
    
                    # %%
                    print(3 + 3)
                    print(4 + 4)
    
                    print(5 + 5)"#
                }
            ]
        );
        
        // Test for repl: run above cells
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(10, 0)..Point::new(10, 3), cx, ReplRunMode::AboveCells);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)"#
                },
                indoc! { r#"
                    # %% [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)"#
                }
            ]
        );
        
        // Test forrepl: run above codes
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(2, 0)..Point::new(2, 0), cx, ReplRunMode::Above);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)"#
                }
            ]
        );
    } 
        
    #[gpui::test]
    fn test_run_all_and_run_above_without_cells(cx: &mut App) {
        // Create a test language
        let test_language = Arc::new(Language::new(
            LanguageConfig {
                name: "TestLang".into(),
                line_comments: vec!["# ".into()],
                ..Default::default()
            },
            None,
        ));
                
        let buffer = cx.new(|cx| {
            Buffer::local(
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)
                    
                    # [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)
    
                    # 
                    print(3 + 3)
                    print(4 + 4)
    
                    print(5 + 5)
    
    
    
                "# },
                cx,
            )
            .with_language(test_language, cx)
        });
        let snapshot = buffer.read(cx).snapshot();
        
        // Test for repl: run all without cells
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(1, 0)..Point::new(1, 0), cx, ReplRunMode::All);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)
                    
                    # [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)
    
                    # 
                    print(3 + 3)
                    print(4 + 4)
    
                    print(5 + 5)"#
                }
            ]
        );
        
        // Test for repl: run above codes
        let (snippets, _) = runnable_ranges(&snapshot, Point::new(10, 0)..Point::new(10, 3), cx, ReplRunMode::Above);
        let snippets = snippets
            .into_iter()
            .map(|range| snapshot.text_for_range(range).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            snippets,
            vec![
                indoc! { r#"
                    # Hello!
                    print("before first cell", 0)
                    
                    # [markdown]
                    # This is some arithmetic
                    print(1 + 1)
                    print(2 + 2)
    
                    # 
                    print(3 + 3)"#
                } 
            ]
        );
    }

    #[gpui::test]
    fn test_block_range_python(cx: &mut App) {
        let python = languages::language("python", tree_sitter_python::LANGUAGE.into());

        // Test function detection
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("def times_two(x):\n    print(x*2)\ntimes_two(3)\n", cx);
            buffer.set_language(Some(python.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        // Cursor inside function body should select entire function
        let range = block_range(&snapshot, Point::new(1, 4)..Point::new(1, 4));
        assert!(range.is_some());
        let range = range.unwrap();
        let text: String = snapshot.text_for_range(range).collect();
        assert_eq!(text, "def times_two(x):\n    print(x*2)");

        // Cursor on standalone statement should select just that statement
        let range = block_range(&snapshot, Point::new(2, 0)..Point::new(2, 0));
        assert!(range.is_some());
        let text: String = snapshot.text_for_range(range.unwrap()).collect();
        assert_eq!(text, "times_two(3)");

        // Test for-loop detection
        let buffer = cx.new(|cx| {
            let mut buffer =
                Buffer::local("for i in range(3):\n    print(i)\nprint(\"done\")\n", cx);
            buffer.set_language(Some(python.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        // Cursor inside for-loop body should select entire for-loop
        let range = block_range(&snapshot, Point::new(1, 4)..Point::new(1, 4));
        assert!(range.is_some());
        let text: String = snapshot.text_for_range(range.unwrap()).collect();
        assert_eq!(text, "for i in range(3):\n    print(i)");

        // Test class detection
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(
                "class Foo:\n    def bar(self):\n        pass\nx = Foo()\n",
                cx,
            );
            buffer.set_language(Some(python.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        // Cursor inside nested method should select entire class (top-level)
        let range = block_range(&snapshot, Point::new(2, 8)..Point::new(2, 8));
        assert!(range.is_some());
        let text: String = snapshot.text_for_range(range.unwrap()).collect();
        assert_eq!(text, "class Foo:\n    def bar(self):\n        pass");

        // Test selection override - when user has a selection, use that instead
        let range = block_range(&snapshot, Point::new(1, 0)..Point::new(2, 12));
        assert!(range.is_some());
        let text: String = snapshot.text_for_range(range.unwrap()).collect();
        // Selection is respected, not expanded to top-level block
        assert_eq!(text, "    def bar(self):\n        pass");
    }    

    #[gpui::test]
    fn test_line_runnable_range(cx: &mut App) {
        let python = languages::language("python", tree_sitter_python::LANGUAGE.into());

        // 复杂测试代码，覆盖所有场景
        let code = indoc::indoc! {r#"
            # This is a comment
            print("First line")

            def func(long_arg_a, long_arc_b,
                long_arc_c):
                print("First line of func")
                for i in range(4):
                    if(i%3==0):
                        print("Inside if")
                    elif(i%3==1):
                        print("Inside elif")
                    else:
                        with open("f.txt") as f:
                            f.readline()
                        print("Inside else")
                    print("Inside for outside if..else")
                    try:
                        x = 1 / (i - 2)
                    except ZeroDivisionError:
                        print("Division by zero")
                    except Exception as e:
                        print("Other error")
                    finally:
                        print("Finally block")
                match status:
                    case 200:
                        print("OK")
                    case 404:
                        print("Not Found")
                    case _:
                        print("Unknown")
                print(
                    "multi-line",
                    "call",
                )

            async def async_func():
                async for j in async_range(3):
                    print(j)
                async with async_open("f.txt") as f:
                    await f.read()

            print("Last line")
        "#};

        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(code, cx);
            buffer.set_language(Some(python.clone()), cx);
            buffer
        });
        let snapshot = buffer.read(cx).snapshot();

        // 辅助闭包：获取指定行的运行文本
        let run_line = |row: u32| -> String {
            let range = line_runnable_range(&snapshot, Point::new(row, 0)..Point::new(row, 0));
            snapshot.text_for_range(range).collect()
        };

        // ---- 规则4：注释行或空行 ----
        assert_eq!(run_line(0), r#"print("First line")"#);

        // 空行（行2）、行3、行4都应该跳转到 def 并返回整个函数定义        
        assert_eq!(
            run_line(2),
            "def func(long_arg_a, long_arc_b,\n    long_arc_c):\n    print(\"First line of func\")\n    for i in range(4):\n        if(i%3==0):\n            print(\"Inside if\")\n        elif(i%3==1):\n            print(\"Inside elif\")\n        else:\n            with open(\"f.txt\") as f:\n                f.readline()\n            print(\"Inside else\")\n        print(\"Inside for outside if..else\")\n        try:\n            x = 1 / (i - 2)\n        except ZeroDivisionError:\n            print(\"Division by zero\")\n        except Exception as e:\n            print(\"Other error\")\n        finally:\n            print(\"Finally block\")\n    match status:\n        case 200:\n            print(\"OK\")\n        case 404:\n            print(\"Not Found\")\n        case _:\n            print(\"Unknown\")\n    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );
        assert!(run_line(2)==run_line(3) && run_line(2)==run_line(4));

        // ---- 普通单行 ----
        assert_eq!(run_line(1), r#"print("First line")"#);

        // ---- 跨行表达式（多行 print 调用）----
        assert_eq!(
            run_line(31),
            "    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );
        assert_eq!(
            run_line(32),
            "    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );
        assert_eq!(
            run_line(33),
            "    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );
        assert_eq!(
            run_line(34),
            "    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );

        // ---- 函数定义块 ----
        assert_eq!(
            run_line(3),
            "def func(long_arg_a, long_arc_b,\n    long_arc_c):\n    print(\"First line of func\")\n    for i in range(4):\n        if(i%3==0):\n            print(\"Inside if\")\n        elif(i%3==1):\n            print(\"Inside elif\")\n        else:\n            with open(\"f.txt\") as f:\n                f.readline()\n            print(\"Inside else\")\n        print(\"Inside for outside if..else\")\n        try:\n            x = 1 / (i - 2)\n        except ZeroDivisionError:\n            print(\"Division by zero\")\n        except Exception as e:\n            print(\"Other error\")\n        finally:\n            print(\"Finally block\")\n    match status:\n        case 200:\n            print(\"OK\")\n        case 404:\n            print(\"Not Found\")\n        case _:\n            print(\"Unknown\")\n    print(\n        \"multi-line\",\n        \"call\",\n    )"
        );

        // ---- for 块 ----
        assert_eq!(
            run_line(6),
            "    for i in range(4):\n        if(i%3==0):\n            print(\"Inside if\")\n        elif(i%3==1):\n            print(\"Inside elif\")\n        else:\n            with open(\"f.txt\") as f:\n                f.readline()\n            print(\"Inside else\")\n        print(\"Inside for outside if..else\")\n        try:\n            x = 1 / (i - 2)\n        except ZeroDivisionError:\n            print(\"Division by zero\")\n        except Exception as e:\n            print(\"Other error\")\n        finally:\n            print(\"Finally block\")"
        );

        // ---- if/elif/else 完整块 ----
        assert_eq!(
            run_line(7),
            "        if(i%3==0):\n            print(\"Inside if\")\n        elif(i%3==1):\n            print(\"Inside elif\")\n        else:\n            with open(\"f.txt\") as f:\n                f.readline()\n            print(\"Inside else\")"
        );
        assert_eq!(run_line(9), run_line(7)); // elif 行应与 if 行返回相同
        assert_eq!(run_line(11), run_line(7)); // else 行应与 if 行返回相同

        // ---- with 块 ----
        assert_eq!(
            run_line(12),
            "            with open(\"f.txt\") as f:\n                f.readline()"
        );

        // ---- try/except/finally 完整块 ----
        assert_eq!(
            run_line(16),
            "        try:\n            x = 1 / (i - 2)\n        except ZeroDivisionError:\n            print(\"Division by zero\")\n        except Exception as e:\n            print(\"Other error\")\n        finally:\n            print(\"Finally block\")"
        );
        assert_eq!(run_line(18), run_line(16)); // except 行
        assert_eq!(run_line(22), run_line(16)); // finally 行

        // ---- match/case 完整块 ----
        assert_eq!(
            run_line(24),
            "    match status:\n        case 200:\n            print(\"OK\")\n        case 404:\n            print(\"Not Found\")\n        case _:\n            print(\"Unknown\")"
        );
        assert_eq!(run_line(25), run_line(24)); // case 行

        // ---- async 函数定义 ----
        assert_eq!(
            run_line(36),
            "async def async_func():\n    async for j in async_range(3):\n        print(j)\n    async with async_open(\"f.txt\") as f:\n        await f.read()"
        );

        // async for
        assert_eq!(
            run_line(37),
            "    async for j in async_range(3):\n        print(j)"
        );

        // async with
        assert_eq!(
            run_line(39),
            "    async with async_open(\"f.txt\") as f:\n        await f.read()"
        );

        // ---- 最后单行 ----
        assert_eq!(run_line(42), r#"print("Last line")"#);

        // ---- 多行括号表达式 ----
        let paren_code = indoc::indoc! {r#"
            (
            df.fillna(0)
                .head()
            )
        "#};
        let buffer2 = cx.new(|cx| {
            let mut buffer = Buffer::local(paren_code, cx);
            buffer.set_language(Some(python.clone()), cx);
            buffer
        });
        let snapshot2 = buffer2.read(cx).snapshot();
        let run_line2 = |row: u32| -> String {
            let range = line_runnable_range(&snapshot2, Point::new(row, 0)..Point::new(row, 0));
            snapshot2.text_for_range(range).collect()
        };
        assert_eq!(
            run_line2(0),
            "(\ndf.fillna(0)\n    .head()\n)"
        );
        assert_eq!(
            run_line2(3),
            "(\ndf.fillna(0)\n    .head()\n)"
        );
    }
}
