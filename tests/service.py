#!/usr/bin/env python3
"""One private-bus, disposable-data check of job execution and restart recovery."""
import ast
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BUS = "io.github.design_nexus.nixe.Transfer1"
OBJECT = "/io/github/design_nexus/nixe/Transfer1"


def call(method, *args):
    result = subprocess.run(
        ["gdbus", "call", "--session", "--dest", BUS, "--object-path", OBJECT,
         "--method", f"{BUS}.{method}", *args],
        text=True, capture_output=True, timeout=8,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    parsed = ast.literal_eval(result.stdout.strip())
    return parsed[0] if parsed else None


def wait(predicate, timeout=15):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        try:
            value = predicate()
            if value:
                return value
        except (RuntimeError, ValueError, SyntaxError):
            pass
        time.sleep(0.1)
    raise AssertionError("transfer service did not reach expected state")


def jobs():
    return {job["id"]: job for job in json.loads(call("ListJobs"))}


with tempfile.TemporaryDirectory(prefix="nixe-transfer-service-") as temp:
    base = Path(temp)
    os.environ["HOME"] = str(base)
    os.environ["XDG_DATA_HOME"] = str(base / "data")
    source = base / "source ☃.txt"
    source.write_text("persistent transfer", encoding="utf-8")
    destination = base / "destination.txt"

    def start():
        return subprocess.Popen([str(ROOT / "target/debug/nixe-transferd")],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    daemon = start()
    try:
        wait(lambda: call("ListJobs"))
        job_id = call("Enqueue", json.dumps({"Local": str(source)}),
                      json.dumps({"Local": str(destination)}))
        wait(lambda: jobs().get(job_id, {}).get("status") == "Completed")
        assert destination.read_text(encoding="utf-8") == "persistent transfer"
        assert jobs()[job_id]["bytes"] > 0

        sync_source = base / "sync source"
        sync_target = base / "sync target"
        sync_source.mkdir()
        sync_target.mkdir()
        (sync_source / "new ☃.txt").write_text("new", encoding="utf-8")
        (sync_target / "old.txt").write_text("old", encoding="utf-8")
        source_json = json.dumps({"Local": str(sync_source)})
        target_json = json.dumps({"Local": str(sync_target)})
        mode_json = "Mirror"
        preview = json.loads(call("PreviewSync", source_json, target_json, mode_json))
        assert {change["action"] for change in preview["changes"]} == {"Copy", "Delete"}
        sync_id = call("EnqueueSync", source_json, target_json, mode_json, preview["fingerprint"])
        wait(lambda: jobs().get(sync_id, {}).get("status") == "Completed")
        assert (sync_target / "new ☃.txt").read_text(encoding="utf-8") == "new"
        assert not (sync_target / "old.txt").exists()

        stale = json.loads(call("PreviewSync", source_json, target_json, mode_json))
        (sync_target / "surprise.txt").write_text("surprise", encoding="utf-8")
        try:
            call("EnqueueSync", source_json, target_json, mode_json, stale["fingerprint"])
            raise AssertionError("stale sync preview accepted")
        except RuntimeError as exc:
            assert "changed since preview" in str(exc)
        assert (sync_target / "surprise.txt").exists()

        db = base / "data/nixe-transfer/connections.db"
        def save_workflow(workflow):
            with sqlite3.connect(db) as conn:
                conn.execute("INSERT OR REPLACE INTO workflows (id,metadata) VALUES (?,?)",
                             (workflow["id"], json.dumps(workflow)))

        filtered_source = base / "filtered source"
        filtered_target = base / "filtered target"
        filtered_source.mkdir()
        filtered_target.mkdir()
        for filename in ("keep.txt", "skip.txt", "photo.png"):
            (filtered_source / filename).write_text(filename, encoding="utf-8")
        for filename in ("stale.txt", "photo.png"):
            (filtered_target / filename).write_text(filename, encoding="utf-8")
        filtered = {
            "id": "abcdef01-0000-4000-8000-000000000001", "name": "Filtered mirror",
            "source": {"Local": str(filtered_source)}, "destination": {"Local": str(filtered_target)},
            "mode": "Mirror", "include": ["*.txt"], "exclude": ["skip.txt"],
            "conflict": "KeepBoth", "checksum": False, "bandwidth_kib": None,
            "baseline_ready": False,
        }
        save_workflow(filtered)
        preview = json.loads(call("PreviewWorkflow", filtered["id"]))
        assert any("keep.txt" in line for line in preview["lines"])
        assert not any("skip.txt" in line for line in preview["lines"])
        filtered_id = call("EnqueueWorkflow", filtered["id"], preview["fingerprint"])
        wait(lambda: jobs().get(filtered_id, {}).get("status") == "Completed")
        assert (filtered_target / "keep.txt").exists()
        assert (filtered_target / "skip.txt").exists() is False
        assert (filtered_target / "photo.png").exists()
        assert not (filtered_target / "stale.txt").exists()

        left = base / "bisync left"
        right = base / "bisync right"
        left.mkdir()
        right.mkdir()
        (left / "left.txt").write_text("left", encoding="utf-8")
        (right / "right.txt").write_text("right", encoding="utf-8")
        bidirectional = {
            "id": "abcdef01-0000-4000-8000-000000000002", "name": "Two-way documents",
            "source": {"Local": str(left)}, "destination": {"Local": str(right)},
            "mode": "Bidirectional", "include": [], "exclude": [],
            "conflict": "KeepBoth", "checksum": False, "bandwidth_kib": None,
            "baseline_ready": False,
        }
        save_workflow(bidirectional)
        preview = json.loads(call("PreviewWorkflow", bidirectional["id"]))
        assert preview["initial"]
        bisync_id = call("EnqueueWorkflow", bidirectional["id"], preview["fingerprint"])
        wait(lambda: jobs().get(bisync_id, {}).get("status") == "Completed")
        assert (left / "right.txt").exists() and (right / "left.txt").exists()
        with sqlite3.connect(db) as conn:
            stored = json.loads(conn.execute("SELECT metadata FROM workflows WHERE id=?", (bidirectional["id"],)).fetchone()[0])
        assert stored["baseline_ready"] is True
        (left / "later-left.txt").write_text("later left", encoding="utf-8")
        (right / "later-right.txt").write_text("later right", encoding="utf-8")
        preview = json.loads(call("PreviewWorkflow", bidirectional["id"]))
        assert not preview["initial"]
        bisync_id = call("EnqueueWorkflow", bidirectional["id"], preview["fingerprint"])
        wait(lambda: jobs().get(bisync_id, {}).get("status") == "Completed")
        assert (left / "later-right.txt").exists() and (right / "later-left.txt").exists()
        (left / "left.txt").write_text("left side changed", encoding="utf-8")
        (right / "left.txt").write_text("right side changed differently", encoding="utf-8")
        preview = json.loads(call("PreviewWorkflow", bidirectional["id"]))
        assert any("conflict" in line.lower() for line in preview["lines"])
        conflict_id = call("EnqueueWorkflow", bidirectional["id"], preview["fingerprint"])
        wait(lambda: jobs().get(conflict_id, {}).get("status") == "Completed")
        contents = {(path.read_text(encoding="utf-8")) for path in left.glob("left.txt*")}
        assert "left side changed" in contents and "right side changed differently" in contents
    finally:
        daemon.terminate()
        daemon.wait(timeout=5)

    db = base / "data/nixe-transfer/connections.db"
    now = int(time.time())
    for ident, state, target in [("queued-restart", "Queued", "queued.txt"),
                                 ("running-restart", "Running", "interrupted.txt"),
                                 ("paused-resume", "Paused", "resumed.txt"),
                                 ("paused-cancel", "Paused", "cancelled.txt")]:
        snapshot = {
            "id": ident, "source": {"Local": str(source)},
            "destination": {"Local": str(base / target)}, "name": source.name,
            "status": state, "retries": 0, "progress": state,
            "created_at": now, "updated_at": now,
        }
        with sqlite3.connect(db) as conn:
            conn.execute("INSERT INTO jobs (id,snapshot,updated_at) VALUES (?,?,?)",
                         (ident, json.dumps(snapshot), now))

    daemon = start()
    try:
        wait(lambda: jobs().get("queued-restart", {}).get("status") == "Completed")
        interrupted = jobs()["running-restart"]["status"]
        assert "Failed" in interrupted and "Interrupted" in interrupted["Failed"]
        assert not (base / "interrupted.txt").exists()
        assert (base / "queued.txt").read_text(encoding="utf-8") == "persistent transfer"
        call("Retry", "running-restart")
        call("Resume", "paused-resume")
        call("Cancel", "paused-cancel")
        wait(lambda: jobs().get("running-restart", {}).get("status") == "Completed")
        wait(lambda: jobs().get("paused-resume", {}).get("status") == "Completed")
        assert jobs()["paused-cancel"]["status"] == "Cancelled"
        assert not (base / "cancelled.txt").exists()
        call("ClearCompleted")
        assert "paused-cancel" in jobs() and "queued-restart" not in jobs()
    finally:
        daemon.terminate()
        daemon.wait(timeout=5)
print("PASS: private-bus transfer, sync workflows, bisync conflicts, recovery, and queue controls")
