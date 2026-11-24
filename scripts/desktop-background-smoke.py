#!/usr/bin/env python3
"""Verify Mesh Forum's native accessibility navigation without activating it."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

APP = "mesh-forum-desktop"


def desktop(*args, value=None):
    result = subprocess.run(
        ["agent-desktop", *args], check=False, capture_output=True, text=True, input=value, timeout=10
    )
    return json.loads(result.stdout)


def foreground_pid():
    return subprocess.check_output(
        ["swift", "-e", "import AppKit; print(NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1)"],
        text=True,
        timeout=10,
    ).strip()


def walk(node):
    yield node
    for child in node.get("children", []):
        yield from walk(child)


def wait_for(name, process, *, role=None, field="name"):
    deadline = time.monotonic() + 15
    last = []
    while process.poll() is None and time.monotonic() < deadline:
        result = desktop("snapshot", "--app", APP, "--max-depth", "16")
        nodes = list(walk(result.get("data", {}).get("tree", {})))
        last = [(node.get("role"), node.get("name")) for node in nodes if node.get("name")]
        for node in nodes:
            actual = node.get(field)
            if field == "value" and isinstance(actual, str):
                actual = actual.rstrip("\n")
            if (role is None or node.get("role") == role) and actual == name:
                return node
        time.sleep(0.2)
    raise RuntimeError(f"Missing {name!r} ({role or field}) in the background window: {last}")


def wait_until_hidden(name, process):
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        result = desktop("snapshot", "--app", APP, "--max-depth", "16")
        tree = result.get("data", {}).get("tree", {})
        if tree and not any(node.get("name") == name for node in walk(tree)):
            return
        time.sleep(0.2)
    raise RuntimeError(f"Still visible after correcting input: {name!r}")


def click(name, process, *, role="button"):
    button = wait_for(name, process, role=role)
    result = desktop("click", button["ref_id"])
    if not result.get("ok"):
        raise RuntimeError(f"Background click failed for {name!r}: {result.get('error')}")


def fill(name, value, process):
    field = wait_for(name, process, role="textfield")
    result = desktop("set-value", field["ref_id"], "--stdin", value=value)
    error = result.get("error", {})
    if not result.get("ok") and error.get("disposition", {}).get("delivery") != "delivered_unverified":
        raise RuntimeError(f"Could not fill {name!r}: {error.get('code', 'unknown error')}")
    if name != "密码":
        wait_for(value, process, role="textfield", field="value")


def check_case(label, process):
    if foreground_pid() == str(process.pid):
        raise RuntimeError(f"Mesh Forum became the foreground application during {label}")
    print(f"PASS: {label}", flush=True)


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main(executable):
    inventory = desktop("list-apps", "--app", APP)
    if not inventory.get("ok") or inventory["data"]["apps"]:
        raise RuntimeError("The test requires an unused mesh-forum-desktop process name")

    with tempfile.TemporaryDirectory(prefix="mesh-forum-background-") as directory:
        env = os.environ.copy()
        env.pop("MESH_FORUM_CAPTURE_DIR", None)
        if capture := env.get("MESH_FORUM_EVAL_CAPTURE_DIR"):
            folder = Path(capture)
            folder.mkdir(parents=True, exist_ok=True)
            env["MESH_FORUM_CAPTURE_DIR"] = str(folder / ("narrow-dark" if env.get("MESH_FORUM_TEST_NARROW") else "wide"))
        launch_env = {
            **env,
            "MESH_FORUM_TEST_BACKGROUND": "1",
            "MESH_FORUM_DATA": directory,
            "MESH_FORUM_LISTEN": "127.0.0.1:0",
        }
        binary = [str(Path(executable).resolve())]
        app = subprocess.Popen(binary, env=launch_env)
        try:
            wait_for("暂无附近帖子。发现设备后会自动更新。", app)
            click("账号", app)
            wait_for("创建账号", app, role="button")
            click("从备份恢复", app)
            wait_for("选择备份文件", app, role="button")
            click("返回账号", app)
            wait_for("创建账号", app, role="button")
            check_case("guest navigation and account recovery entry", app)

            click("节点", app)
            wait_for("附近节点", app)
            click("暂停上传", app)
            wait_for("恢复上传", app, role="button")
            click("恢复上传", app)
            wait_for("暂停上传", app, role="button")
            click("存储", app)
            wait_for("自动清理", app)
            wait_for("离线书签", app)
            for label in ("篇数", "容量（MiB）", "闲置天数"):
                wait_for(label, app, role="textfield")
            check_case("discovery, reversible sharing control and cache settings", app)

            click("写帖子", app)
            click("发布", app)
            wait_for("请填写帖子标题", app)
            wait_for("请填写帖子正文", app)
            fill("帖子标题", "后台验收帖子", app)
            wait_until_hidden("请填写帖子标题", app)
            click("发布", app)
            wait_for("请填写帖子正文", app)
            fill("写下你的帖子", "后台验收正文", app)
            wait_until_hidden("请填写帖子正文", app)
            click("设置", app)
            click("English", app, role="switch")
            wait_for("Appearance and language", app)
            click("Dark" if env.get("MESH_FORUM_TEST_DARK") else "Light", app, role="switch")
            click("Done", app)
            wait_for("后台验收正文", app, role="textfield", field="value")
            click("Settings", app)
            click("简体中文", app, role="switch")
            click("完成", app)
            wait_for("后台验收帖子", app, role="textfield", field="value")
            check_case("live language changes preserve draft title and body", app)
            click("预览", app)
            wait_for("后台验收正文", app, field="value")
            click("撰写", app)
            wait_for("后台验收正文", app, field="value")
            click("发布", app)
            wait_for("返回草稿", app, role="button")
            click("返回草稿", app)
            wait_for("后台验收正文", app, field="value")
            check_case("draft validation, preview and guest publish gate", app)

            click("发布", app)
            fill("显示名称", "验收用户", app)
            fill("密码", "forum-evaluation-fixture-password", app)
            click("创建账号", app)
            wait_for("后台验收正文", app, field="value")
            click("发布", app)
            wait_for("已发布到本机", app)
            wait_for("后台验收帖子", app, role="statictext")
            click("编辑", app)
            fill("写下你的帖子", "后台验收正文修订", app)
            click("发布", app)
            wait_for("后台验收正文修订", app, field="value")
            check_case("account creation, signed publish and edit", app)

            click("设置", app)
            click("English", app, role="switch")
            click("Done", app)
            wait_for("后台验收正文修订", app, field="value")
            click("Find posts", app)
            wait_for("Searches only posts and cache on this device.", app)
            fill("Search title, author, or body", "no matching post fixture", app)
            wait_for("No matching posts on this device.", app)
            fill("Search title, author, or body", "后台验收正文修订", app)
            click("后台验收帖子 · My posts", app)
            wait_for("后台验收正文修订", app, field="value")
            wait_for("Available offline", app)
            check_case("English local search opens a Chinese post without changing the text", app)

            stop(app)
            app = subprocess.Popen(binary, env=launch_env)
            wait_for("New post", app, role="button")
            click("Settings", app)
            wait_for("Appearance and language", app)
            check_case("English preference survives a restart", app)
        finally:
            stop(app)
    print("PASS: Background desktop acceptance. Mesh Forum stayed in the background at checkpoints.")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "target/debug/mesh-forum-desktop")
