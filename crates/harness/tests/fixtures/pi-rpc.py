#!/usr/bin/env python3
import json
import sys


def send(value):
    print(json.dumps(value), flush=True)


def response(request, data=None, success=True, error=None):
    value = {"type": "response", "id": request.get("id"), "success": success}
    if data is not None:
        value["data"] = data
    if error is not None:
        value["error"] = error
    send(value)


current_prompt_mode = ""
# The session file the fake is sitting on; switch_session repoints it and
# fork/clone derive the copy's file from it, so tests can assert which
# session a forked run resumes.
current_session = "/tmp/fake-pi-session.jsonl"

for line in sys.stdin:
    request = json.loads(line)
    command = request.get("type")
    if command == "get_state":
        response(request, {
            "sessionId": "native-session",
            "sessionFile": current_session,
            "model": {"provider": "test", "id": "native", "contextWindow": 100000},
        })
    elif command == "get_available_models":
        response(request, {"models": [{
            "provider": "test", "id": "native", "name": "Native Test",
            "reasoning": True,
            "thinkingLevelMap": {"minimal": "minimal", "low": "low", "medium": "medium", "high": "high"},
        }]})
    elif command == "get_commands":
        response(request, {"commands": [{"name": "compact", "description": "Compact context"}]})
    elif command == "get_session_stats":
        response(request, {"contextUsage": {"tokens": 25, "contextWindow": 100000}})
    elif command == "switch_session":
        session_path = request.get("sessionPath", "")
        if "cancelled" in session_path:
            response(request, {"cancelled": True})
        else:
            current_session = session_path
            response(request, {})
    elif command == "get_fork_messages":
        if "no_fork" in current_session:
            response(request, success=False, error="fork unavailable")
        else:
            response(request, {"messages": [
                {"entryId": "e1", "text": "first turn"},
                {"entryId": "e2", "text": "second turn"},
            ]})
    elif command in ("fork", "clone"):
        entry_id = request.get("entryId")
        if command == "fork" and entry_id == "missing":
            response(request, success=False, error="no such entry")
            continue
        current_session = current_session + ".fork"
        response(request, {})
    elif command == "set_model":
        # The distinct window marker lets tests prove the switch landed: the
        # startup get_state/settlement stats report 100000.
        response(request, {"model": {
            "provider": request.get("provider", "test"),
            "id": request.get("modelId", "native"),
            "contextWindow": 777000,
        }})
    elif command == "set_thinking_level":
        response(request, {})
    elif command == "steer":
        if "steer_reject" in current_prompt_mode:
            response(request, success=False, error="steer rejected")
            send({"type": "agent_settled"})
        else:
            response(request, {})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "steered"}})
            send({"type": "agent_settled"})
        current_prompt_mode = ""
    elif command == "abort":
        if "abort_reverse" in current_prompt_mode:
            send({"type": "agent_settled"})
            response(request, {})
        elif "abort_error" in current_prompt_mode:
            response(request, success=False, error="abort request failed")
            send({"type": "agent_settled"})
        else:
            response(request, {})
            send({"type": "agent_settled"})
        current_prompt_mode = ""
    elif command == "prompt":
        message = request.get("message", "")
        if "prompt_error" in message:
            response(request, success=False, error="prompt rejected by provider")
            continue
        if "crash_eof" in message:
            response(request, {"agentInvoked": True})
            sys.exit(1)
        if "local_only" in message:
            send({"type": "command_output", "text": "local output"})
            response(request, {"agentInvoked": False})
            continue
        # Echo harness: slash-resolution tests assert what the harness put on
        # the wire by having the resolved message come back as a delta —
        # "WIRE:…" bodies from expanded templates, "/…" commands verbatim.
        if message.startswith(("WIRE:", "/")):
            response(request, {"agentInvoked": True})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": message}})
            send({"type": "agent_settled"})
            continue

        response(request, {"agentInvoked": True})
        send({"type": "agent_start"})

        if "extension_error" in message:
            send({"type": "extension_error", "error": "extension hook failed"})
            send({"type": "agent_settled"})
        elif "standard_agent_end" in message:
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "before"}})
            send({"type": "agent_end", "messages": [], "willRetry": False})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "after"}})
            send({"type": "agent_settled"})
        elif "intermediate_settle" in message:
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "delta1"}})
            send({"type": "agent_settled", "isTerminal": False})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "delta2"}})
            send({"type": "agent_settled", "isTerminal": True})
        elif "tool_update_dedup" in message:
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": {"path": "src/main.rs"}})
            send({"type": "tool_execution_update", "toolCallId": "tool-1", "toolName": "read", "partialResult": {"content": "ok"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "result": {"content": "ok"}, "isError": False})
            send({"type": "agent_settled"})
        elif "tool_update_progress" in message:
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": {"path": "src/main.rs"}})
            send({"type": "tool_execution_update", "toolCallId": "tool-1", "toolName": "read", "partialResult": {"content": "partial"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "result": {"content": "final"}, "isError": False})
            send({"type": "agent_settled"})
        elif "tool_update_error" in message:
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": {"path": "src/main.rs"}})
            send({"type": "tool_execution_update", "toolCallId": "tool-1", "toolName": "read", "partialResult": {"content": "started"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "result": None, "isError": True})
            send({"type": "agent_settled"})
        elif "todo_flow" in message:
            # Pi's todo tool is CRUD-shaped: start args carry the action, the
            # authoritative task list only lands on the end's details.tasks.
            send({"type": "tool_execution_start", "toolCallId": "td-1", "toolName": "todo",
                  "args": {"action": "create", "subject": "Inspect renderer", "status": "in_progress", "activeForm": "inspecting"}})
            send({"type": "tool_execution_end", "toolCallId": "td-1", "toolName": "todo", "isError": False,
                  "result": {"content": [{"type": "text", "text": "Created #1: Inspect renderer (pending)"}],
                             "details": {"action": "create", "nextId": 2,
                                         "tasks": [{"id": 1, "subject": "Inspect renderer", "status": "in_progress", "activeForm": "inspecting"}]}}})
            send({"type": "tool_execution_start", "toolCallId": "td-2", "toolName": "todo",
                  "args": {"action": "create", "subject": "Write tests"}})
            send({"type": "tool_execution_end", "toolCallId": "td-2", "toolName": "todo", "isError": False,
                  "result": {"content": [{"type": "text", "text": "Created #2: Write tests (pending)"}],
                             "details": {"action": "create", "nextId": 3,
                                         "tasks": [{"id": 1, "subject": "Inspect renderer", "status": "completed", "activeForm": "inspecting"},
                                                    {"id": 2, "subject": "Write tests", "status": "pending"}]}}})
            send({"type": "agent_settled"})
        elif "extension_select" in message:
            # An extension dialog: emit the request, then keep the turn alive
            # until the client's extension_ui_response arrives (the response
            # branch below echoes the answer back as a text delta so tests can
            # observe what was sent without the turn settling first).
            send({"type": "extension_ui_request", "id": "sel-1", "method": "select",
                  "title": "Allow dangerous command?", "options": ["Allow", "Block"]})
            current_prompt_mode = "awaiting_extension"
            continue
        elif "extension_confirm" in message:
            send({"type": "extension_ui_request", "id": "cfm-1", "method": "confirm",
                  "title": "Clear session?", "message": "All messages will be lost."})
            current_prompt_mode = "awaiting_extension"
            continue
        elif "structured_content" in message:
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "bash", "args": {"command": "ls"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "bash", "result": {"content": [{"type": "text", "text": "cleaned tool output"}]}, "isError": False})
            send({"type": "agent_settled"})
        elif "post_settle_next" in message:
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "turn2_done"}})
            send({"type": "agent_settled"})
        elif "auto_title" in message:
            # Pi renames its session mid-run; the repeat must not re-emit.
            send({"type": "session_info_changed", "name": "Pi picked a title"})
            send({"type": "session_info_changed", "name": "Pi picked a title"})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "done"}})
            send({"type": "agent_settled"})
        elif "abort" in message or "steer" in message:
            current_prompt_mode = message
            continue
        else:
            send({"type": "message_start", "message": {"role": "assistant"}})
            send({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "delta": "checking"}})
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": {"path": "src/main.rs"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "result": {"content": "ok"}, "isError": False})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "done"}})
            send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}], "usage": {"input": 20, "output": 5, "totalTokens": 25}}})
            send({"type": "agent_settled"})
    elif command == "extension_ui_response":
        # The answer to a pending dialog: surface what the client sent as a
        # text delta (so tests observe the exact wire value) and settle the
        # turn the request was holding open.
        if current_prompt_mode == "awaiting_extension":
            answer = request.get("value", request.get("confirmed"))
            if request.get("cancelled"):
                answer = "CANCELLED"
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": f"ANSWER:{answer}"}})
            send({"type": "agent_settled"})
            current_prompt_mode = ""
    else:
        response(request, success=False, error=f"unsupported command: {command}")
