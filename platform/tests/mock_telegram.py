#!/usr/bin/env python3
"""Small stateful Telegram Bot API double used by the platform E2E test."""

from __future__ import annotations

import argparse
import json
import threading
import time
from email.parser import BytesParser
from email.policy import default
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse


class State:
    def __init__(self) -> None:
        self.condition = threading.Condition()
        self.updates: list[dict[str, Any]] = []
        self.sent: list[dict[str, Any]] = []
        self.requests: list[dict[str, Any]] = []
        self.topics: dict[int, dict[str, Any]] = {}
        self.next_message_id = 10_000
        self.next_topic_id = 42


STATE = State()


class Handler(BaseHTTPRequestHandler):
    server_version = "KASMockTelegram/1"

    def log_message(self, format: str, *args: object) -> None:
        return

    def read_request(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length", "0"))
        if length == 0:
            return {}
        content_type = self.headers.get("Content-Type", "")
        payload = self.rfile.read(length)
        if content_type.startswith("multipart/form-data"):
            message = BytesParser(policy=default).parsebytes(
                f"Content-Type: {content_type}\r\n\r\n".encode() + payload
            )
            value: dict[str, Any] = {}
            for part in message.iter_parts():
                name = part.get_param("name", header="content-disposition")
                if not name:
                    continue
                content = part.get_payload(decode=True) or b""
                filename = part.get_filename()
                if filename is None:
                    value[name] = content.decode()
                else:
                    value[name] = {
                        "filename": filename,
                        "content_type": part.get_content_type(),
                        "size": len(content),
                    }
            return value
        value = json.loads(payload)
        if not isinstance(value, dict):
            raise ValueError("request body must be a JSON object")
        return value

    def write_json(self, status: int, value: Any) -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        path = urlparse(self.path).path
        if path == "/health":
            self.write_json(200, {"ok": True})
            return
        if path == "/test/sent":
            with STATE.condition:
                sent = list(STATE.sent)
            self.write_json(200, sent)
            return
        if path == "/test/requests":
            with STATE.condition:
                requests = list(STATE.requests)
            self.write_json(200, requests)
            return
        if path == "/test/topics":
            with STATE.condition:
                topics = list(STATE.topics.values())
            self.write_json(200, topics)
            return
        self.write_json(404, {"ok": False, "description": "not found"})

    def do_POST(self) -> None:
        path = urlparse(self.path).path
        try:
            body = self.read_request()
        except (json.JSONDecodeError, ValueError) as error:
            self.write_json(400, {"ok": False, "description": str(error)})
            return

        if path == "/test/enqueue":
            with STATE.condition:
                STATE.updates.append(body)
                STATE.updates.sort(key=lambda update: int(update["update_id"]))
                STATE.condition.notify_all()
            self.write_json(201, body)
            return

        method = path.rsplit("/", 1)[-1]
        if not path.startswith("/bot"):
            self.write_json(404, {"ok": False, "description": "not found"})
            return
        with STATE.condition:
            STATE.requests.append({"method": method, "request": body})
            STATE.condition.notify_all()
        if method == "getMe":
            self.write_json(
                200,
                {
                    "ok": True,
                    "result": {
                        "id": 7_000_001,
                        "is_bot": True,
                        "first_name": "KAS E2E",
                        "username": "kas_e2e_bot",
                    },
                },
            )
            return
        if method == "getUpdates":
            offset = int(body.get("offset", 0))
            timeout = min(float(body.get("timeout", 0)), 1.0)
            deadline = time.monotonic() + timeout
            with STATE.condition:
                updates = [
                    update
                    for update in STATE.updates
                    if int(update["update_id"]) >= offset
                ]
                while not updates and time.monotonic() < deadline:
                    STATE.condition.wait(deadline - time.monotonic())
                    updates = [
                        update
                        for update in STATE.updates
                        if int(update["update_id"]) >= offset
                    ]
            self.write_json(200, {"ok": True, "result": updates})
            return
        if method in {
            "sendMessage",
            "sendPhoto",
            "sendVideo",
            "sendAudio",
            "sendAnimation",
            "sendDocument",
        }:
            with STATE.condition:
                message_id = STATE.next_message_id
                STATE.next_message_id += 1
                STATE.sent.append({"message_id": message_id, "request": body})
                STATE.condition.notify_all()
            self.write_json(
                200,
                {
                    "ok": True,
                    "result": {
                        "message_id": message_id,
                        "message_thread_id": (
                            int(body["message_thread_id"])
                            if body.get("message_thread_id") is not None
                            else None
                        ),
                        "chat": {"id": int(body["chat_id"])},
                        "from": {
                            "id": 7_000_001,
                            "is_bot": True,
                            "first_name": "KAS E2E",
                            "username": "kas_e2e_bot",
                        },
                        "text": body.get("text", ""),
                        "caption": body.get("caption"),
                    },
                },
            )
            return
        if method == "answerCallbackQuery":
            self.write_json(200, {"ok": True, "result": True})
            return
        if method == "editMessageText":
            message_id = int(body["message_id"])
            chat_id = int(body["chat_id"])
            with STATE.condition:
                for sent in STATE.sent:
                    if (
                        int(sent["message_id"]) == message_id
                        and int(sent["request"]["chat_id"]) == chat_id
                    ):
                        sent["request"]["text"] = body.get("text", "")
                        sent["request"]["reply_markup"] = body.get("reply_markup")
                        break
                STATE.condition.notify_all()
            self.write_json(
                200,
                {
                    "ok": True,
                    "result": {
                        "message_id": message_id,
                        "chat": {"id": chat_id},
                        "from": {
                            "id": 7_000_001,
                            "is_bot": True,
                            "first_name": "KAS E2E",
                            "username": "kas_e2e_bot",
                        },
                        "text": body.get("text", ""),
                    },
                },
            )
            return
        if method == "createForumTopic":
            with STATE.condition:
                topic_id = STATE.next_topic_id
                STATE.next_topic_id += 1
                topic = {
                    "message_thread_id": topic_id,
                    "name": body["name"],
                    "icon_color": body.get("icon_color", 7322096),
                    "closed": False,
                }
                STATE.topics[topic_id] = topic
                STATE.condition.notify_all()
            self.write_json(200, {"ok": True, "result": topic})
            return
        if method == "editForumTopic":
            topic_id = int(body["message_thread_id"])
            with STATE.condition:
                topic = STATE.topics.get(topic_id)
                if topic is None:
                    self.write_json(
                        400,
                        {"ok": False, "description": "forum topic not found"},
                    )
                    return
                if "name" in body:
                    topic["name"] = body["name"]
                STATE.condition.notify_all()
            self.write_json(200, {"ok": True, "result": True})
            return
        if method == "closeForumTopic":
            topic_id = int(body["message_thread_id"])
            with STATE.condition:
                topic = STATE.topics.get(topic_id)
                if topic is None:
                    self.write_json(
                        400,
                        {"ok": False, "description": "forum topic not found"},
                    )
                    return
                topic["closed"] = True
                STATE.condition.notify_all()
            self.write_json(200, {"ok": True, "result": True})
            return
        self.write_json(
            404, {"ok": False, "description": f"unsupported method {method}"}
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    ThreadingHTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
