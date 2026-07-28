const PAGE: &str = include_str!("../web/index.html");

#[test]
fn page_contains_both_pane_modes_and_gate_controls() {
    for marker in [
        "mode-messages",
        "mode-interactive",
        "data-gate-mode",
        "approval-dialog",
        "pane-tree",
        "message-list",
        "terminal-snapshot",
        "operation-log",
    ] {
        assert!(PAGE.contains(marker), "missing UI marker: {marker}");
    }
}

#[test]
fn operation_log_switches_between_critical_and_full_records() {
    for marker in [
        r#"id="log-critical""#,
        r#"id="log-full""#,
        r#"operationLogMode === "full" ? latestState?.fullLog : latestState?.operations"#,
        r#"operationLogMode = value;"#,
    ] {
        assert!(
            PAGE.contains(marker),
            "missing operation log marker: {marker}"
        );
    }
}

#[test]
fn operation_log_uses_an_accessible_close_icon() {
    assert!(PAGE.contains(
        r#"id="close-operation-log" class="dialog-close" type="button" aria-label="关闭" title="关闭">×</button>"#
    ));
}

#[test]
fn safe_read_tools_render_as_clean_pane_messages() {
    for marker in [
        r#"record.tool === "list-directory""#,
        r#"record.tool === "read-file""#,
        r#"record.tool === "find-files""#,
        r#"record.tool === "search-text""#,
        "record.result?.structuredContent",
        "structured.entries.join",
        "structured.content",
        "structured.files.join",
        "structured.matches.join",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing safe read message marker: {marker}"
        );
    }
}

#[test]
fn safe_git_tools_render_as_clean_pane_messages() {
    for marker in [
        r#"record.tool === "git-status""#,
        r#"record.tool === "git-diff""#,
        r#"record.tool === "git-log""#,
        r#"record.tool === "git-show""#,
        r#"record.tool?.startsWith("git-")"#,
        "structured?.output",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing safe Git message marker: {marker}"
        );
    }
}

#[test]
fn page_polls_snapshots_and_uses_the_control_api() {
    for marker in [
        "/api/state",
        "/capture",
        "/keys",
        "/api/gate",
        "/api/approvals/",
        "250",
        "Enter 或切换 pane",
        "输入已清空",
    ] {
        assert!(PAGE.contains(marker), "missing behavior marker: {marker}");
    }
}

#[test]
fn capture_response_is_scoped_to_the_requested_pane_and_mode() {
    let capture = PAGE
        .split_once("async function refreshCapture()")
        .expect("refreshCapture function")
        .1
        .split_once("function sendKey")
        .expect("sendKey function")
        .0;
    for marker in [
        "const paneId = selectedPane;",
        "/api/panes/${encodeURIComponent(paneId)}/capture",
        r#"mode === "interactive" && selectedPane === paneId"#,
    ] {
        assert!(
            capture.contains(marker),
            "missing scoped capture marker: {marker}"
        );
    }
    assert_eq!(
        capture
            .matches(r#"mode === "interactive" && selectedPane === paneId"#)
            .count(),
        2,
        "capture success and error rendering must both be scoped"
    );
}

#[test]
fn pane_switch_clears_old_history_before_requesting_the_new_history() {
    let select = PAGE
        .split_once("function selectPane(paneId)")
        .expect("selectPane function")
        .1
        .split_once("function renderTopology")
        .expect("selectPane function end")
        .0;
    let clear = select
        .find("renderMessages([], true);")
        .expect("loading history");
    let refresh = select.find("void refreshState();").expect("state refresh");
    assert!(clear < refresh);
    assert!(PAGE.contains("正在载入这个 pane 的消息…"));
}

#[test]
fn stale_message_responses_are_never_rendered_for_another_pane() {
    let refresh = PAGE
        .split_once("async function refreshState()")
        .expect("refreshState function")
        .1
        .split_once("async function refreshCapture()")
        .expect("refreshState function end")
        .0;
    for marker in [
        "const requestedPane = selectedPane;",
        "requestedPane === selectedPane",
        "renderMessages(state.messages || []);",
        "renderMessages([], true);",
    ] {
        assert!(
            refresh.contains(marker),
            "missing pane scope marker: {marker}"
        );
    }
}

#[test]
fn message_mode_contains_a_single_line_command_composer() {
    for marker in [
        r#"id="command-composer""#,
        r#"id="command-input""#,
        r#"id="command-send""#,
        "/api/panes/${encodeURIComponent(paneId)}/commands",
        "requestSubmit()",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing command composer marker: {marker}"
        );
    }
}

#[test]
fn shell_cwd_sits_between_message_history_and_command_composer() {
    let message_view = PAGE
        .split_once(r#"<section id="messages-view""#)
        .expect("messages view")
        .1
        .split_once(r#"<section id="interactive-view""#)
        .expect("messages view end")
        .0;
    let history = message_view
        .find(r#"id="message-list""#)
        .expect("message list");
    let cwd = message_view.find(r#"id="messages-cwd""#).expect("cwd");
    let composer = message_view
        .find(r#"id="command-composer""#)
        .expect("command composer");
    assert!(history < cwd && cwd < composer);

    for marker in [
        "function isShellCommand(command)",
        "paneInfo?.id === selectedPane",
        "isShellCommand(paneInfo.currentCommand)",
        "elements.messagesCwd.hidden = !showCwd",
        "elements.messagesCwdPath.textContent = showCwd ? paneInfo.currentPath : \"\"",
    ] {
        assert!(PAGE.contains(marker), "missing cwd marker: {marker}");
    }
}

#[test]
fn command_composer_rejects_embedded_newlines() {
    assert!(PAGE.contains(r#"command.includes("\r") || command.includes("\n")"#));
}

#[test]
fn command_composer_only_restores_focus_in_message_mode() {
    assert!(PAGE.contains(r#"if (mode === "messages") elements.commandInput.focus();"#));
}

#[test]
fn message_refresh_preserves_manual_scroll_position() {
    assert!(PAGE.contains(
        "elements.messageList.scrollTop = followLatest ? elements.messageList.scrollHeight : scrollTop;"
    ));
}

#[test]
fn running_command_notice_uses_a_compact_accessible_spinner() {
    for marker in [
        r#"const interferenceText = runningSources.length ? "命令执行中" : "";"#,
        r#"source === "你" ? "你提交的命令正在执行" : `进程 ${source} 提交的命令正在执行`"#,
        "elements.messagesInterference.title = interferenceTitle;",
        ".interference::before",
        "@keyframes spin",
        "@media (prefers-reduced-motion: reduce)",
    ] {
        assert!(PAGE.contains(marker), "missing running indicator: {marker}");
    }
    assert!(!PAGE.contains("AI 正在这个 pane"));
}

#[test]
fn gate_approval_is_attached_above_the_composer() {
    assert!(PAGE.contains(r#"id="approval-dialog""#));
    assert!(PAGE.contains(r#"class="approval-popover""#));
    let stack = PAGE
        .split_once(r#"<div class="composer-stack">"#)
        .expect("composer stack")
        .1
        .split_once(r#"<section id="interactive-view""#)
        .expect("composer stack end")
        .0;
    let approval = stack.find(r#"id="approval-dialog""#).expect("approval");
    let composer = stack
        .find(r#"id="command-composer""#)
        .expect("command composer");
    assert!(approval < composer);
    assert!(PAGE.contains(".approval-popover {\n        width: 100%;"));
    assert!(!PAGE.contains("bottom: 72px"));
    assert!(PAGE.contains("elements.approval.hidden = false"));
    let render_approval = PAGE
        .split_once("function renderApproval(pending)")
        .expect("renderApproval function")
        .1
        .split_once("function renderOperations")
        .expect("renderApproval function end")
        .0;
    assert!(!render_approval.contains("showModal"));
    assert!(!render_approval.contains(".close()"));
}

#[test]
fn delayed_approval_hides_immediately_and_cannot_drop_its_refresh() {
    for marker in [
        r#"let stateRefreshQueued = false;"#,
        r#"let decidingApprovalId = "";"#,
        r#"let approvalCooldown = false;"#,
        "const decidedApprovalIds = new Set();",
        "pending?.find((item) => !decidedApprovalIds.has(item.id))",
        "if (!id || decidingApprovalId || approvalCooldown) return;",
        "decidingApprovalId = id;",
        "approvalCooldown = true;",
        "if (focused && elements.approval.contains(focused)) focused.blur();",
        "}, 600);",
        "decidedApprovalIds.add(id);",
        "renderApproval(latestState?.pending || []);",
        "latestState.pending = (latestState.pending || []).filter((record) => record.id !== id);",
        "stateRefreshQueued = true;",
        "if (stateRefreshQueued)",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing delayed approval guard: {marker}"
        );
    }
}

#[test]
fn gate_control_uses_an_accessible_three_mode_switch() {
    for marker in [
        r#"id="gate-modes""#,
        r#"data-gate-mode="off""#,
        r#"data-gate-mode="tools""#,
        r#"data-gate-mode="approval""#,
        r#"role="group" aria-label="Gate 模式""#,
        ".gate-modes::before",
        r#".gate-modes[data-mode="tools"]::before"#,
        r#".gate-modes[data-mode="approval"]::before"#,
        r#"elements.gateModes.dataset.mode = value;"#,
        r#"body: { mode: button.dataset.gateMode }"#,
        r#"button.setAttribute("aria-pressed", String(active))"#,
    ] {
        assert!(
            PAGE.contains(marker),
            "missing Gate switch marker: {marker}"
        );
    }
}

#[test]
fn command_cards_use_unlabeled_streams_and_collapsed_run_details() {
    for marker in [
        r#"node("details", "command-details")"#,
        r#"node("summary", "", "运行详情")"#,
        r#"node("span", "command-prompt", "$")"#,
        r#"output.setAttribute("aria-label", "输出")"#,
    ] {
        assert!(
            PAGE.contains(marker),
            "missing command card marker: {marker}"
        );
    }
    assert!(!PAGE.contains(r#"body.appendChild(node("p", "card-label", "命令"))"#));
    assert!(!PAGE.contains(r#"snapshot.outputTruncated ? "输出（已截断）" : "完整输出""#));
}

#[test]
fn command_cards_use_safe_basic_shell_highlighting() {
    for marker in [
        "function renderCommand(command)",
        "document.createTextNode",
        r#"className = "shell-command""#,
        r#"className = "shell-option""#,
        r#"className = "shell-string""#,
        r#"className = "shell-operator""#,
        "const commandText = renderCommand(command)",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing shell highlight marker: {marker}"
        );
    }
}

#[test]
fn long_message_inputs_and_outputs_can_expand_without_resetting_on_refresh() {
    for marker in [
        "const expandedContent = new Set()",
        "const LONG_CONTENT_CHARS = 1200",
        "const LONG_CONTENT_LINES = 12",
        r#"node("details", "long-content")"#,
        "details.open = expandedContent.has(key)",
        r#"collapsibleContent(record, "command", "命令", command, commandText)"#,
        r#"collapsibleContent(record, "output", "输出", outputText, output)"#,
        r#"collapsibleContent(record, "input", "输入", input, node("pre", "input-text", input))"#,
        r#"appendLabeledPre(body, record, "result", "结果", result, "result-text")"#,
    ] {
        assert!(
            PAGE.contains(marker),
            "missing collapsible content marker: {marker}"
        );
    }
}

#[test]
fn message_view_uses_a_flat_wide_layout() {
    let card_css = PAGE
        .split_once(".message-card {")
        .expect("message card CSS")
        .1
        .split_once('}')
        .expect("message card CSS end")
        .0;
    assert!(card_css.contains("max-width: 1120px"));
    assert!(card_css.contains("border: 0"));
    assert!(!card_css.contains("border-radius"));

    let command_css = PAGE
        .split_once(".command-line {")
        .expect("command line CSS")
        .1
        .split_once('}')
        .expect("command line CSS end")
        .0;
    assert!(command_css.contains("border: 0"));
    assert!(!command_css.contains("border-radius"));
}

#[test]
fn page_never_renders_remote_text_as_html() {
    assert!(PAGE.contains("textContent"));
    assert!(!PAGE.contains("innerHTML"));
    assert!(!PAGE.contains("insertAdjacentHTML"));
}

#[test]
fn page_uses_the_light_theme_contract() {
    assert!(PAGE.contains(r#"name="color-scheme" content="light""#));
    assert!(PAGE.contains("--bg: #ffffff"));
    assert!(PAGE.contains("--panel-2: #f7f7f8"));
    assert!(PAGE.contains("color: #854d0e"));
    assert!(PAGE.contains(".truncated { color: #854d0e; }"));
    assert!(PAGE.contains("color: #b91c1c"));
    assert!(PAGE.contains("border-left: 3px solid var(--text)"));
    assert!(PAGE.contains("background: #ececf1"));
    assert!(!PAGE.contains("--bg: #0b0d10"));
}

#[test]
fn ai_pause_is_an_inline_composer_banner_with_a_clear_action() {
    for marker in [
        r#"id="ai-pause-banner""#,
        "AI 操作已暂停",
        "无法确认 pane",
        "恢复 AI 操作",
        r#"api("/api/ai-pause/clear""#,
        "renderAiPause(state.aiPause)",
        ".ai-pause-banner[hidden] { display: none; }",
    ] {
        assert!(PAGE.contains(marker), "missing AI pause marker: {marker}");
    }
}
