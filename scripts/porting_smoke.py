#!/usr/bin/env python3
"""Reproducible stdio MCP probe: analyze, generate, compare, restart, inspect."""
import argparse
import json
import os
from pathlib import Path
import selectors
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent


class Client:
    def __init__(self, binary, data):
        env = dict(os.environ, REFORGE_DATA_DIR=str(data))
        self.log = open(data / "mcp-server.stderr", "a", encoding="utf-8")
        self.process = subprocess.Popen(
            [str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.log, env=env, text=True, bufsize=1,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.counter = 0
        initialized = self.call("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "reforge-porting-smoke", "version": "1"},
        })
        assert initialized["serverInfo"]["name"] == "reforge-mcp"
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method, parameters, timeout=120):
        self.counter += 1
        self.send({"jsonrpc": "2.0", "id": self.counter, "method": method, "params": parameters})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not self.selector.select(max(0, deadline - time.monotonic())):
                continue
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError("Server exited; inspect mcp-server.stderr")
            result = json.loads(line)
            if result.get("id") != self.counter:
                continue
            if "error" in result:
                raise RuntimeError(result["error"])
            return result["result"]
        raise TimeoutError(method)

    def tool(self, name, arguments):
        result = self.call("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(result)
        return json.loads(next(item["text"] for item in result["content"] if item["type"] == "text"))

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)
        self.selector.close()
        self.log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/reforge-mcp")
    parser.add_argument("--data-dir", type=Path, default=ROOT / ".reforge-demo")
    arguments = parser.parse_args()
    binary = arguments.binary.resolve()
    data = arguments.data_dir.resolve()
    data.mkdir(parents=True, exist_ok=True)
    client = Client(binary, data)
    try:
        capabilities = client.tool("get_porting_capabilities", {})
        assert capabilities["stage"] == "c_vertical_slice"
        job = client.tool("analyze_porting_project", {"source_path": str(ROOT / "fixtures/porting/arithmetic")})
        assert job["state"] == "analyzed", job
        job_id = job["job_id"]
        client.tool("create_porting_plan", {"job_id": job_id})
        context = client.tool("get_porting_context", {"job_id": job_id, "function_name": "score"})
        assert set(context["function"]["dependencies"]) == {"clamp", "magnitude"}
        client.tool("generate_rust", {"job_id": job_id})
        cases = [[-200, 0], [-100, 1], [-7, 3], [0, 0], [8, -1], [200, 10]]
        verified = client.tool("verify_port", {"job_id": job_id, "entry_function": "score", "cases": cases})
        assert verified["state"] == "completed", verified
        assert [case["source"]["stdout"].strip() for case in verified["verifications"][-1]["cases"]] == ["199", "201", "17", "-1", "15", "210"]
        blocked = client.tool("analyze_porting_project", {"source_path": str(ROOT / "fixtures/porting/unsupported")})
        assert blocked["state"] == "blocked" and blocked["unsupported"]
    finally:
        client.close()
    resumed = Client(binary, data)
    try:
        report = resumed.tool("get_porting_report", {"job_id": job_id})
        assert report["state"] == "completed" and len(report["verifications"]) == 1
        assert any(item["job_id"] == job_id for item in resumed.tool("list_porting_jobs", {})["jobs"])
        output = data / "report.json"
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"job_id": job_id, "state": report["state"], "matched_cases": len(cases), "restart_resume": True, "unsupported_pointer": "blocked", "report": str(output)}, ensure_ascii=False))
    finally:
        resumed.close()


if __name__ == "__main__":
    main()
