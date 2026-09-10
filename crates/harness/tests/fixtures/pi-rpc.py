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

for line in sys.stdin:
    request = json.loads(line)
    command = request.get("type")
    if command == "get_state":
        response(request, {
            "sessionId": "native-session",
            "sessionFile": "/tmp/fake-pi-session.jsonl",
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
            response(request, {})
    elif command in ("set_model", "set_thinking_level"):
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
        elif "structured_content" in message:
            send({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "bash", "args": {"command": "ls"}})
            send({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "bash", "result": {"content": [{"type": "text", "text": "cleaned tool output"}]}, "isError": False})
            send({"type": "agent_settled"})
        elif "post_settle_next" in message:
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "turn2_done"}})
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
        pass
    else:
        response(request, success=False, error=f"unsupported command: {command}")
