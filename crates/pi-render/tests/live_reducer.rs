use pi_render::{
    Block, ConversationDocument, ConversationItem, ImageState, LiveAssistantUpdate, LiveBlockKind,
    LiveEvent, LivePhase, LiveSessionReducer, MarkdownBlock, Message, MessageRole, MinimapNode,
    ModelRef, ToolOutput, ToolStatus,
};
use serde_json::json;
use std::sync::Arc;

#[test]
fn assembles_multiple_blocks_and_message_end_is_authoritative() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply_batch([
        LiveEvent::AgentStart,
        LiveEvent::MessageStart {
            message: json!({"role":"assistant","content":[]}),
        },
        LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockStart {
            index: 1,
            kind: LiveBlockKind::Text,
        }),
        LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
            index: 1,
            kind: LiveBlockKind::Text,
            delta: "draft".into(),
        }),
        LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockStart {
            index: 0,
            kind: LiveBlockKind::Thinking,
        }),
        LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockEnd {
            index: 0,
            kind: LiveBlockKind::Thinking,
            content: json!("final thought"),
        }),
    ]);
    let draft = reducer.document();
    assert!(
        matches!(&draft.messages[0].blocks[0], Block::Thinking(text) if text == "final thought")
    );
    assert!(
        matches!(&draft.messages[0].blocks[1], Block::Markdown(text) if text.source == "draft")
    );

    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id":"authoritative",
            "role":"assistant",
            "content":[{"type":"text","text":"final snapshot"}]
        }),
    });
    let final_document = reducer.document();
    assert_eq!(final_document.messages[0].id, "authoritative");
    assert!(
        matches!(&final_document.messages[0].blocks[0], Block::Markdown(text) if text.source == "final snapshot")
    );
}

#[test]
fn live_assistant_preserves_model_metadata_from_start_and_end() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageStart {
        message: json!({
            "role":"assistant",
            "provider":"provider-one",
            "model":"model-one",
            "content":[]
        }),
    });
    reducer.apply(LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
        index: 0,
        kind: LiveBlockKind::Text,
        delta: "draft".to_owned(),
    }));
    assert_eq!(
        reducer.document().messages[0].model,
        Some(ModelRef {
            provider: "provider-one".to_owned(),
            id: "model-one".to_owned(),
        })
    );

    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id":"authoritative",
            "role":"assistant",
            "provider":"provider-two",
            "model":"model-two",
            "content":"final"
        }),
    });
    assert_eq!(
        reducer.document().messages[0].model,
        Some(ModelRef {
            provider: "provider-two".to_owned(),
            id: "model-two".to_owned(),
        })
    );
}

#[test]
fn user_start_and_end_upsert_by_stable_run_identity() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageStart {
        message: json!({
            "role":"user",
            "content":"hello",
            "timestamp":"start-only"
        }),
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "role":"user",
            "content":[{"type":"text","text":"hello"}],
            "timestamp":"authoritative",
            "providerMetadata":{"different":true}
        }),
    });
    let document = reducer.document();
    assert_eq!(document.messages.len(), 1);
    assert_eq!(
        document.messages[0].timestamp.as_deref(),
        Some("authoritative")
    );
}

#[test]
fn fresh_identity_update_survives_the_next_live_document_snapshot() {
    let mut reducer = LiveSessionReducer::empty("fresh-1", "");
    reducer.set_session_identity("real-session", "C:/sessions/real.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageStart {
        message: json!({"role":"user","content":"hello"}),
    });

    let document = reducer.document();
    assert_eq!(document.session_id, "real-session");
    assert_eq!(
        document.source_path,
        std::path::PathBuf::from("C:/sessions/real.jsonl")
    );
    assert_eq!(document.messages.len(), 1);
}

#[test]
fn live_image_is_stable_across_nested_start_flat_end_and_assistant_stream() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9ZQmcAAAAASUVORK5CYII=";
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageStart {
        message: json!({
            "role":"user",
            "content":[
                {"type":"text","text":"look"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}
            ]
        }),
    });
    let started = reducer.document();
    assert_eq!(started.messages.len(), 1);
    assert!(matches!(
        &started.messages[0].blocks[1],
        Block::Image(image) if image.state == ImageState::Inline && image.mime_type.as_deref() == Some("image/png")
    ));
    assert!(!started.text_snapshot().contains(png));

    reducer.apply(LiveEvent::MessageStart {
        message: json!({"role":"assistant","content":[]}),
    });
    reducer.apply(LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
        index: 0,
        kind: LiveBlockKind::Text,
        delta: "working".to_owned(),
    }));
    let streaming = reducer.document();
    assert!(matches!(streaming.messages[0].blocks[1], Block::Image(_)));

    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "role":"user",
            "content":[
                {"type":"text","text":"look"},
                {"type":"image","data":png,"mimeType":"image/png"}
            ]
        }),
    });
    let ended = reducer.document();
    assert_eq!(
        ended
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1
    );
    assert!(matches!(ended.messages[0].blocks[1], Block::Image(_)));
}

#[test]
fn optimistic_running_then_agent_start_still_advances_run_identity() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_running();
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"role":"user","content":"first"}),
    });
    reducer.apply(LiveEvent::AgentSettled);

    reducer.set_running();
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"role":"user","content":"second"}),
    });

    let document = reducer.document();
    assert_eq!(document.messages.len(), 2);
    assert!(
        matches!(&document.messages[0].blocks[0], Block::Markdown(text) if text.source == "first")
    );
    assert!(
        matches!(&document.messages[1].blocks[0], Block::Markdown(text) if text.source == "second")
    );
}

#[test]
fn same_run_distinct_user_messages_do_not_overwrite_each_other() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_running();
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"role":"user","content":"original prompt"}),
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"role":"user","content":"steer message"}),
    });

    let document = reducer.document();
    assert_eq!(document.messages.len(), 2);
}

#[test]
fn completed_history_is_arc_cached_across_draft_frames() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id":"completed",
            "role":"assistant",
            "content":[{"type":"text","text":"fixed"}]
        }),
    });
    let completed = reducer.document().messages[0].clone();
    for delta in ["a", "b", "c"] {
        reducer.apply(LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
            index: 0,
            kind: LiveBlockKind::Text,
            delta: delta.to_owned(),
        }));
        let frame = reducer.document();
        assert!(Arc::ptr_eq(&completed, &frame.messages[0]));
    }
}

#[test]
fn static_history_items_and_minimap_are_reused_across_draft_frames() {
    let history_message = Arc::new(Message {
        id: "history".to_owned(),
        role: MessageRole::Assistant,
        timestamp: None,
        label: None,
        model: None,
        written_files: Vec::new(),
        blocks: vec![Block::Markdown(MarkdownBlock {
            source: "fixed history".to_owned(),
        })],
    });
    let history = ConversationDocument {
        session_id: "s".to_owned(),
        source_path: "fixture.jsonl".into(),
        cwd: std::env::temp_dir(),
        messages: Arc::from([history_message.clone()]),
        items: Arc::from([ConversationItem::Message(history_message.clone())]),
        minimap: Arc::from([MinimapNode {
            message_id: "history".to_owned(),
            turn: 0,
            role: MessageRole::Assistant,
            label: "history".to_owned(),
            level: None,
        }]),
        diagnostics: Arc::from([]),
    };
    let mut reducer = LiveSessionReducer::new(history);
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageStart {
        message: json!({"role":"assistant","content":[]}),
    });
    for delta in ["a", "b", "c"] {
        reducer.apply(LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
            index: 0,
            kind: LiveBlockKind::Text,
            delta: delta.to_owned(),
        }));
        let frame = reducer.document();
        assert!(matches!(
            &frame.items[0],
            ConversationItem::Message(message) if Arc::ptr_eq(message, &history_message)
        ));
        assert_eq!(frame.minimap[0].message_id, "history");
        assert_eq!(frame.minimap[0].turn, 0);
    }
}

#[test]
fn history_and_live_minimap_turns_are_continuous() {
    let user = Arc::new(Message {
        id: "history-user".to_owned(),
        role: MessageRole::User,
        timestamp: None,
        label: None,
        model: None,
        written_files: Vec::new(),
        blocks: vec![Block::Markdown(MarkdownBlock {
            source: "old".to_owned(),
        })],
    });
    let history = ConversationDocument {
        session_id: "s".to_owned(),
        source_path: "fixture.jsonl".into(),
        cwd: std::env::temp_dir(),
        messages: Arc::from([user.clone()]),
        items: Arc::from([ConversationItem::Message(user)]),
        minimap: Arc::from([MinimapNode {
            message_id: "history-user".to_owned(),
            turn: 1,
            role: MessageRole::User,
            label: "old".to_owned(),
            level: None,
        }]),
        diagnostics: Arc::from([]),
    };
    let mut reducer = LiveSessionReducer::new(history);
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"id":"live-user","role":"user","content":"new"}),
    });
    let document = reducer.document();
    assert_eq!(document.minimap[0].turn, 1);
    assert_eq!(document.minimap[1].turn, 2);
}

#[test]
fn tool_progress_replaces_accumulated_result_and_queue_snapshot_replaces() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "role":"assistant",
            "content":[{"type":"toolCall","id":"call","name":"bash","arguments":{"command":"test"}}]
        }),
    });
    reducer.apply(LiveEvent::ToolExecutionStart {
        id: "call".into(),
        name: "bash".into(),
        arguments: json!({"command":"test"}),
    });
    reducer.apply(LiveEvent::ToolExecutionUpdate {
        id: "call".into(),
        name: "bash".into(),
        arguments: json!({"command":"test"}),
        partial_result: json!({"content":[{"type":"text","text":"old"}]}),
    });
    reducer.apply(LiveEvent::ToolExecutionUpdate {
        id: "call".into(),
        name: "bash".into(),
        arguments: json!({"command":"test"}),
        partial_result: json!({"content":[{"type":"text","text":"new cumulative"}]}),
    });
    reducer.apply(LiveEvent::QueueUpdate {
        steering: vec!["one".into(), "two".into()],
        follow_up: vec!["later".into()],
    });
    reducer.apply(LiveEvent::QueueUpdate {
        steering: vec!["replacement".into()],
        follow_up: Vec::new(),
    });

    assert_eq!(reducer.steering_queue(), ["replacement"]);
    assert!(reducer.follow_up_queue().is_empty());
    let document = reducer.document();
    let Block::Tool(tool) = &document.messages[0].blocks[0] else {
        panic!("expected tool")
    };
    assert_eq!(tool.status, ToolStatus::Pending);
    assert!(matches!(&tool.output[0], ToolOutput::Ansi(output) if output.text == "new cumulative"));
}

#[test]
fn active_tail_process_stays_expanded_until_settled() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({"id":"u","role":"user","content":"question"}),
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id":"mixed",
            "role":"assistant",
            "content":[
                {"type":"thinking","thinking":"reasoning"},
                {"type":"text","text":"provisional answer"}
            ]
        }),
    });
    let running = reducer.document();
    assert!(matches!(
        &running.items[1],
        ConversationItem::Process(group) if !group.collapsible && group.message_count == 1
    ));
    assert_eq!(running.minimap.len(), 1);

    reducer.apply(LiveEvent::AgentSettled);
    let settled = reducer.document();
    assert!(matches!(
        &settled.items[1],
        ConversationItem::Process(group) if group.collapsible
    ));
    assert!(matches!(
        &settled.items[2],
        ConversationItem::Message(message) if message.id == "mixed"
    ));
    assert_eq!(settled.minimap.len(), 2);
}

#[test]
fn agent_end_is_not_idle_abort_waits_for_settled() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply(LiveEvent::AgentStart);
    assert_eq!(reducer.phase(), LivePhase::Running);
    reducer.set_stopping();
    reducer.apply(LiveEvent::AgentEnd);
    assert_eq!(reducer.phase(), LivePhase::Stopping);
    let outcome = reducer.apply(LiveEvent::AgentSettled);
    assert!(outcome.settled);
    assert_eq!(reducer.phase(), LivePhase::Idle);
}

#[test]
fn abort_error_restores_running_only_while_stopping() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_stopping();
    assert!(reducer.restore_running_if_stopping());
    assert_eq!(reducer.phase(), LivePhase::Running);

    reducer.apply(LiveEvent::AgentSettled);
    assert!(!reducer.restore_running_if_stopping());
    assert_eq!(reducer.phase(), LivePhase::Idle);
}

#[test]
fn out_of_order_and_burst_updates_degrade_safely() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    let mut events = Vec::new();
    for _ in 0..2048 {
        events.push(LiveEvent::MessageUpdate(LiveAssistantUpdate::BlockDelta {
            index: 0,
            kind: LiveBlockKind::Text,
            delta: "x".into(),
        }));
    }
    events.push(LiveEvent::AgentEnd);
    events.push(LiveEvent::AgentSettled);
    let outcome = reducer.apply_batch(events);
    assert!(outcome.settled);
    assert_eq!(reducer.phase(), LivePhase::Idle);
    let document = reducer.document();
    assert!(
        matches!(&document.messages[0].blocks[0], Block::Markdown(text) if text.source.len() == 2048)
    );
    assert!(!document.diagnostics.is_empty());
}

/// R25：流式段的过程性负载必须有聚合上限。
///
/// 逐条上限拦不住"每条都合规、加起来几百 MB"这一类；多会话之后这个数字还要再乘以
/// 并行会话数。
#[test]
fn live_segment_payload_stays_within_budget_and_releases_the_oldest_first() {
    const OUTPUT: usize = 8 * 1024;
    // 预算刻意取在"恰好放得下一条、放不下两条"之间：小于单条负载的预算只能证明
    // "全被释放了"，证明不了释放顺序是从最旧开始的。
    //
    // 一条消息实际占**两份** OUTPUT —— 渲染出来的工具输出，加上还留在 `tools[*].result`
    // 里的那份原始结果。预算算的是实际驻留内存，所以门槛要按 2×OUTPUT 取。
    const BUDGET: usize = 20 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(BUDGET);
    for index in 0..3 {
        finish_tool_turn(&mut reducer, &format!("t{index}"), &"x".repeat(OUTPUT));
    }

    let document = reducer.document();
    assert_eq!(document.messages.len(), 3);
    let retained: usize = document
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(
        retained <= BUDGET,
        "流式段保留了 {retained} 字节，超过预算 {BUDGET}"
    );

    assert!(
        tool_output_is_released(&document.messages[0]),
        "最旧的工具输出应先被释放"
    );
    assert!(
        !tool_output_is_released(&document.messages[2]),
        "预算之内的最新一条必须完整保留"
    );
}

/// 工具结果回来会触发整段重渲染 —— 重渲染是从原始 value 重建的，会把释放过的负载
/// 原样带回来。不在重渲染之后重新压一次，预算就只在"没有工具结果回来"时有效。
#[test]
fn re_rendering_completed_tools_does_not_resurrect_released_payload() {
    const OUTPUT: usize = 8 * 1024;
    // 预算刻意取在"恰好放得下一条、放不下两条"之间：小于单条负载的预算只能证明
    // "全被释放了"，证明不了释放顺序是从最旧开始的。
    //
    // 一条消息实际占**两份** OUTPUT —— 渲染出来的工具输出，加上还留在 `tools[*].result`
    // 里的那份原始结果。预算算的是实际驻留内存，所以门槛要按 2×OUTPUT 取。
    const BUDGET: usize = 20 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(BUDGET);
    for index in 0..3 {
        finish_tool_turn(&mut reducer, &format!("t{index}"), &"x".repeat(OUTPUT));
    }
    assert!(tool_output_is_released(&reducer.document().messages[0]));

    // 再来一条工具结果：`rerender_completed_tools` 会把所有带工具卡片的完成消息重渲染。
    reducer.apply(LiveEvent::ToolExecutionEnd {
        id: "t2".to_owned(),
        name: "read".to_owned(),
        result: json!({"content": "y".repeat(OUTPUT)}),
        is_error: false,
    });

    let document = reducer.document();
    let retained: usize = document
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(
        retained <= BUDGET,
        "重渲染之后仍须在预算内，实际 {retained} 字节"
    );
    assert!(
        tool_output_is_released(&document.messages[0]),
        "重渲染不得把已经释放的负载复活"
    );
}

fn finish_tool_turn(reducer: &mut LiveSessionReducer, id: &str, output: &str) {
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::ToolExecutionEnd {
        id: id.to_owned(),
        // 刻意不用 "bash"：那条路径会把输出解析成 ANSI，本用例只关心体积。
        name: "read".to_owned(),
        result: json!({ "content": output }),
        is_error: false,
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id": id,
            "role": "assistant",
            "content": [{"type":"toolCall","id": id, "name":"read","arguments":{"path": id}}]
        }),
    });
}

fn tool_output_is_released(message: &Message) -> bool {
    message.blocks.iter().any(|block| match block {
        Block::Tool(tool) => {
            tool.output.len() == 1
                && matches!(&tool.output[0], ToolOutput::Text(text) if text.contains("已释放以控制内存占用"))
        }
        _ => false,
    })
}

/// R25 整改（codex P1）：只裁渲染结果等于只把统计做小。
///
/// 最尖锐的一格是「渲染后根本没有 bytes」的图片：无效、超限或已脱敏的图片经
/// `parse_image` 出来 `bytes` 是 `None`，`payload_bytes` 一律记 0 —— 可那一大坨 base64
/// 还完整躺在 `CompletedMessage.value` 里。只按渲染侧记账的话，一串这样的消息能让
/// 预算永远触发不了，而内存一路涨上去。
#[test]
fn raw_image_payload_alone_is_enough_to_trigger_a_release() {
    const IMAGE_DATA: usize = 16 * 1024;
    const BUDGET: usize = 4 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    // 先用大预算收下它，确保这坨数据确实被完整留了下来。
    reducer.set_payload_budget(1024 * 1024);
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id": "shot",
            "role": "user",
            "content": [{
                "type": "image",
                // 不是合法 PNG：`parse_image` 会给出一个没有 bytes 的占位块。
                "source": {"type":"base64","media_type":"image/png","data": "A".repeat(IMAGE_DATA)}
            }]
        }),
    });

    let rendered: usize = reducer
        .document()
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert_eq!(
        rendered, 0,
        "渲染侧看不到任何负载 —— 这正是只按渲染结果记账会漏掉这坨内存的原因"
    );
    let raw_before = reducer.live_raw_bytes();
    assert!(
        raw_before >= IMAGE_DATA,
        "原始副本里应当确实存着那坨图片数据，实际 {raw_before}"
    );

    // 收紧预算：光凭原始副本就必须触发释放，不需要任何工具输出来把它顶穿。
    reducer.set_payload_budget(BUDGET);
    let raw_after = reducer.live_raw_bytes();
    assert!(
        raw_after < raw_before,
        "原始副本必须跟着缩小，实际从 {raw_before} 变成 {raw_after}"
    );
    assert!(
        raw_after <= BUDGET,
        "原始副本仍然超预算（{raw_after} 字节）—— 内存并没有真的有界"
    );
}

/// 工具结果的原始副本同样要跟着释放。
#[test]
fn releasing_payload_also_drops_the_raw_tool_results_behind_it() {
    const OUTPUT: usize = 8 * 1024;
    const BUDGET: usize = 12 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(1024 * 1024);
    for index in 0..3 {
        finish_tool_turn(&mut reducer, &format!("t{index}"), &"x".repeat(OUTPUT));
    }
    let raw_before = reducer.live_raw_bytes();
    assert!(
        raw_before >= 3 * OUTPUT,
        "三条大输出应当都还在原始副本里，实际 {raw_before}"
    );

    reducer.set_payload_budget(BUDGET);
    let rendered: usize = reducer
        .document()
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(rendered <= BUDGET, "渲染结果应在预算内，实际 {rendered}");
    let raw_after = reducer.live_raw_bytes();
    assert!(
        raw_after <= BUDGET,
        "原始工具结果仍然超预算（{raw_after} 字节）—— 只裁了渲染结果"
    );
}

/// R25 三轮整改（codex P2）：一堆"单条都很短"的输出，加起来照样要能压回预算内。
///
/// 逐条比大小的判据会把每条短输出都跳过，几万条的总量于是永远超预算而一条都释放不掉，
/// 聚合上限直接失效 —— 判据必须落在整条消息上。
#[test]
fn many_short_tool_outputs_are_still_released_in_aggregate() {
    const PIECES: usize = 60;
    const PIECE: usize = 90;
    const BUDGET: usize = 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(1024 * 1024);
    let content: Vec<_> = (0..PIECES)
        .map(|index| json!({"type":"text","text": format!("{index}{}", "y".repeat(PIECE))}))
        .collect();
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::ToolExecutionEnd {
        id: "many".to_owned(),
        name: "read".to_owned(),
        result: json!({ "content": content }),
        is_error: false,
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id": "many",
            "role": "assistant",
            "content": [{"type":"toolCall","id":"many","name":"read","arguments":{}}]
        }),
    });
    // 再来一条，好让最旧那条落进"该释放"的范围。
    finish_tool_turn(&mut reducer, "tail", "z");

    reducer.set_payload_budget(BUDGET);
    let rendered: usize = reducer
        .document()
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(
        rendered <= BUDGET,
        "{PIECES} 条短输出加起来仍超预算（{rendered} 字节）—— 逐条比大小把它们全跳过了"
    );
}

/// R25 三轮整改（codex P2）：纯文本会话不该被拖进预算。
///
/// 预算从来不释放用户 Query 与最终 Answer 的正文。把它们算进来只会让长文本会话永远
/// "超预算"，于是每插一条消息都全量重扫一遍却一个字节也释放不掉。
#[test]
fn text_only_conversations_do_not_drag_the_budget_over() {
    const TEXT: usize = 32 * 1024;
    const BUDGET: usize = 4 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(BUDGET);
    for index in 0..3 {
        reducer.apply(LiveEvent::AgentStart);
        reducer.apply(LiveEvent::MessageEnd {
            message: json!({
                "id": format!("m{index}"),
                "role": "assistant",
                "content": [{"type":"text","text": format!("{index}{}", "答".repeat(TEXT))}]
            }),
        });
    }

    let document = reducer.document();
    assert_eq!(document.messages.len(), 3);
    for (index, message) in document.messages.iter().enumerate() {
        let Block::Markdown(markdown) = &message.blocks[0] else {
            panic!("第 {index} 条应仍是正文块");
        };
        assert!(
            markdown.source.chars().count() > TEXT,
            "对话正文必须原样保留，第 {index} 条实际只剩 {} 字",
            markdown.source.chars().count()
        );
    }
}

/// R25 五轮整改（codex P2）：短输出不该被换成更长的占位。
///
/// 流式段的释放此前直接调 `release_payload`，绕过了整条消息的大小判据 —— 一条 `"ok"`
/// 被换成一整句更长的说明：有用的输出丢了，内存一个字节没省，对应的原始结果那边还会
/// 因为"换了不划算"而拒绝替换，两头落空。
#[test]
fn short_live_outputs_survive_when_releasing_them_would_not_save_anything() {
    const BUDGET: usize = 4 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(1024 * 1024);
    // 最旧一条只有两个字节的输出；随后一条巨大的把预算顶穿。
    finish_tool_turn(&mut reducer, "tiny", "ok");
    finish_tool_turn(&mut reducer, "huge", &"x".repeat(32 * 1024));

    reducer.set_payload_budget(BUDGET);
    let document = reducer.document();
    let Block::Tool(tiny) = &document.messages[0].blocks[0] else {
        panic!("最旧一条应仍是工具卡片");
    };
    assert!(
        matches!(&tiny.output[0], ToolOutput::Text(text) if text == "ok"),
        "换掉它是净亏 —— 短输出必须原样留着，实际是 {:?}",
        tiny.output[0]
    );
}

/// 结构化 `details`（编辑类工具的整份 patch）也要计入并跟着释放。
///
/// 它常常比输出本身还大，而 `crates/ui` / `crates/app` 对它零引用 —— 不算不放，
/// 一篇满是编辑结果的历史能在"预算通过"的同时大幅超出上限。
#[test]
fn tool_details_are_counted_and_released_with_the_output() {
    const PATCH: usize = 24 * 1024;
    const BUDGET: usize = 4 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.set_payload_budget(1024 * 1024);
    reducer.apply(LiveEvent::AgentStart);
    reducer.apply(LiveEvent::ToolExecutionEnd {
        id: "edit".to_owned(),
        name: "edit".to_owned(),
        result: json!({
            "content": "done",
            "details": {"patch": "@@ -1 +1 @@\n".to_owned() + &"+line\n".repeat(PATCH / 6)}
        }),
        is_error: false,
    });
    reducer.apply(LiveEvent::MessageEnd {
        message: json!({
            "id": "edit",
            "role": "assistant",
            "content": [{"type":"toolCall","id":"edit","name":"edit","arguments":{}}]
        }),
    });
    finish_tool_turn(&mut reducer, "tail", "z");

    let before: usize = reducer
        .document()
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(
        before >= PATCH,
        "patch 应当被计入过程性负载，实际只数出 {before}"
    );

    reducer.set_payload_budget(BUDGET);
    let document = reducer.document();
    let after: usize = document
        .messages
        .iter()
        .map(|message| pi_render::payload_bytes(message))
        .sum();
    assert!(after <= BUDGET, "释放后仍超预算：{after} 字节");
    let Block::Tool(edit) = &document.messages[0].blocks[0] else {
        panic!("应仍是工具卡片");
    };
    assert!(edit.details.is_none(), "结构化详情应当跟着输出一起放掉");
}

/// R25 七轮整改（codex P1）：没有对应完成消息的工具结果也要被计入并释放。
///
/// 工具结果常常先于 `MessageEnd` 到达，消息也可能永远不来（run 被取消）。那些结果
/// 一直挂在 `tools` 里，既不被计入也永远释放不掉 —— 足够让 reducer 在"预算通过"的
/// 同时一路涨上去。
#[test]
fn orphan_tool_results_are_counted_and_released() {
    const RESULT: usize = 16 * 1024;
    const BUDGET: usize = 4 * 1024;

    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    // 预算**一开始就设好**，之后不再碰它：这条用例要证的是孤儿结果到达时能自己
    // 触发压制，而不是"外部又调了一次 set_payload_budget 才顺带被裁掉"。
    reducer.set_payload_budget(BUDGET);
    reducer.apply(LiveEvent::AgentStart);
    // 只发工具结果，**不发** MessageEnd：这些结果没有任何完成消息引用它们，
    // 因此 `rerender_completed_tools` 里没有任何卡片会被重渲染。
    for index in 0..4 {
        reducer.apply(LiveEvent::ToolExecutionEnd {
            id: format!("orphan-{index}"),
            name: "read".to_owned(),
            result: json!({ "content": "o".repeat(RESULT) }),
            is_error: false,
        });
    }

    let after = reducer.live_raw_bytes();
    assert!(
        after <= BUDGET,
        "孤儿工具结果仍然超预算（{after} 字节）—— 计入了却没人触发压制，等于没有上界"
    );
}

#[test]
fn a_background_subagent_result_renders_as_a_card_live_not_just_after_reload() {
    // 内核用 `pi.sendMessage({ customType: "subagent-result", .. })` 把后台任务的结果
    // 送回父会话；在 agent 层它就是一条 `role: "custom"` 消息，带着 customType 与 details。
    // 实时渲染必须和从会话文件回看得到**同一种块**，否则同一条结果在流式时是纯文本、
    // 重开会话后才变成卡片，任务面板也要等读盘才认得它。
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply_batch([
        LiveEvent::AgentStart,
        LiveEvent::MessageEnd {
            message: json!({
                "role": "custom",
                "id": "c1",
                "customType": "subagent-result",
                "display": true,
                "content": "[Subagent \"scout\" a1b2c3d4 completed]\n\n找到三处调用点",
                "details": {
                    "type": "scout",
                    "description": "找调用点",
                    "status": "completed",
                    "turnCount": 3,
                    "outputFile": "/tmp/pi-agent-outputs/a1b2c3d4e5f6.log"
                }
            }),
        },
    ]);
    let document = reducer.document();
    let card = document
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .find_map(|block| match block {
            Block::Subagent(card) => Some(card.clone()),
            _ => None,
        })
        .expect("实时路径也必须还原成子代理卡片");
    assert_eq!(card.agent_type, "scout");
    assert_eq!(card.status, pi_render::SubagentStatus::Completed);
    assert_eq!(card.result, "找到三处调用点");
    assert_eq!(card.stats.turn_count, Some(3));

    // 任务面板读的是同一份文档，所以流式期间就能列出这条任务。
    let tasks = pi_render::collect_tasks(&document);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].agent_id.as_deref(), Some("a1b2c3d4e5f6"));
    assert_eq!(tasks[0].status, pi_render::SubagentStatus::Completed);
}

#[test]
fn other_custom_messages_still_render_as_plain_content_live() {
    let mut reducer = LiveSessionReducer::empty("s", "fixture.jsonl");
    reducer.apply_batch([LiveEvent::MessageEnd {
        message: json!({
            "role": "custom",
            "id": "c1",
            "customType": "some-other-extension",
            "content": "普通自定义消息",
        }),
    }]);
    let document = reducer.document();
    assert!(
        !document
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .any(|block| matches!(block, Block::Subagent(_)))
    );
    assert_eq!(document.messages[0].role, MessageRole::Custom);
}
