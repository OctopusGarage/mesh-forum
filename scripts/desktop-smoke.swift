import AppKit
import ApplicationServices
import Carbon

let background = ProcessInfo.processInfo.environment["MESH_FORUM_TEST_BACKGROUND"] == "1"

struct SmokeError: Error, CustomStringConvertible {
    var description: String
}

func attribute(_ element: AXUIElement, _ key: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, key as CFString, &value) == .success ? value : nil
}

func descendants(_ element: AXUIElement, depth: Int = 0) -> [AXUIElement] {
    guard depth < 16 else { return [] }
    let children = attribute(element, "AXChildren") as? [AXUIElement] ?? []
    return [element] + children.flatMap { descendants($0, depth: depth + 1) }
}

func names(_ element: AXUIElement) -> [String] {
    ["AXTitle", "AXDescription", "AXValue", "AXPlaceholderValue"]
        .compactMap { attribute(element, $0) as? String }
}

func waitFor<T>(_ description: String, _ check: () -> T?) throws -> T {
    let deadline = Date().addingTimeInterval(12)
    repeat {
        if let value = check() { return value }
        Thread.sleep(forTimeInterval: 0.12)
    } while Date() < deadline
    throw SmokeError(description: "Timed out waiting for \(description)")
}

func app(_ process: Process) -> AXUIElement {
    let element = AXUIElementCreateApplication(process.processIdentifier)
    AXUIElementSetMessagingTimeout(element, 0.5)
    return element
}

func find(_ process: Process, role: String?, matching: (String) -> Bool) -> AXUIElement? {
    var level = attribute(app(process), "AXWindows") as? [AXUIElement] ?? []
    for _ in 0..<16 where !level.isEmpty {
        if let match = level.first(where: {
            (role == nil || attribute($0, "AXRole") as? String == role)
                && names($0).contains(where: matching)
        }) {
            return match
        }
        level = level.flatMap { element -> [AXUIElement] in
            let role = attribute(element, "AXRole") as? String
            if role == "AXOutline" || role == "AXTable" || role == "AXBrowser" {
                return []
            }
            return attribute(element, "AXChildren") as? [AXUIElement] ?? []
        }
    }
    return nil
}

func press(_ process: Process, _ label: String) throws {
    if !background {
        NSRunningApplication(processIdentifier: process.processIdentifier)?.activate(options: [])
        Thread.sleep(forTimeInterval: 0.15)
    }
    _ = try waitFor("button \(label) in process \(process.processIdentifier)") {
        guard let button = find(process, role: "AXButton", matching: { $0 == label })
                ?? find(process, role: "AXLink", matching: { $0 == label }),
              attribute(button, "AXEnabled") as? Bool != false,
              AXUIElementPerformAction(button, kAXPressAction as CFString) == .success
        else { return nil as Bool? }
        return true
    }
}

func enter(_ process: Process, _ placeholder: String, _ value: String, multiline: Bool = false, secure: Bool = false) throws {
    let field = try waitFor("input \(placeholder)") {
        find(process, role: secure ? nil : (multiline ? "AXTextArea" : "AXTextField"), matching: { $0 == placeholder })
    }
    let result = AXUIElementSetAttributeValue(field, kAXValueAttribute as CFString, value as CFTypeRef)
    if result == .success && secure { return }
    if result == .success,
       (try? waitFor("input value") { find(process, role: nil, matching: { $0 == value }) }) != nil {
        return
    }
    if background {
        throw SmokeError(description: "Could not set field \(placeholder) through Accessibility, AX error \(result.rawValue)")
    }
    guard let application = NSRunningApplication(processIdentifier: process.processIdentifier),
          let point = attribute(field, "AXPosition")
    else { throw SmokeError(description: "Cannot focus \(placeholder), AX error \(result.rawValue)") }
    var origin = CGPoint.zero
    guard CFGetTypeID(point) == AXValueGetTypeID(),
          AXValueGetValue(point as! AXValue, .cgPoint, &origin)
    else { throw SmokeError(description: "No screen coordinates for \(placeholder)") }
    application.activate(options: [])
    Thread.sleep(forTimeInterval: 0.3)
    let location = CGPoint(x: origin.x + 15, y: origin.y + 12)
    for type: CGEventType in [.leftMouseDown, .leftMouseUp] {
        CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: location, mouseButton: .left)?.postToPid(process.processIdentifier)
    }
    Thread.sleep(forTimeInterval: 0.15)
    let utf16 = Array(value.utf16)
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: down)
        event?.keyboardSetUnicodeString(stringLength: utf16.count, unicodeString: utf16)
        event?.postToPid(process.processIdentifier)
    }
    if secure { return }
    _ = try waitFor("entered text \(placeholder)") { find(process, role: nil, matching: { $0 == value }) }
}

func nativeKey(_ code: CGKeyCode, flags: CGEventFlags = [], text: String? = nil) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)
        event?.flags = flags
        if let text {
            let units = Array(text.utf16)
            event?.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        }
        event?.post(tap: .cghidEventTap)
    }
}

func clickNative(_ element: AXUIElement) throws {
    guard let rawPosition = attribute(element, "AXPosition"),
          let rawSize = attribute(element, "AXSize"),
          CFGetTypeID(rawPosition) == AXValueGetTypeID(),
          CFGetTypeID(rawSize) == AXValueGetTypeID()
    else { throw SmokeError(description: "Native button has no screen bounds") }
    var origin = CGPoint.zero
    var size = CGSize.zero
    guard AXValueGetValue(rawPosition as! AXValue, .cgPoint, &origin),
          AXValueGetValue(rawSize as! AXValue, .cgSize, &size)
    else { throw SmokeError(description: "Could not read native button bounds") }
    let center = CGPoint(x: origin.x + size.width / 2, y: origin.y + size.height / 2)
    for event in [CGEventType.leftMouseDown, .leftMouseUp] {
        CGEvent(mouseEventSource: nil, mouseType: event, mouseCursorPosition: center, mouseButton: .left)?.post(tap: .cghidEventTap)
        Thread.sleep(forTimeInterval: 0.05)
    }
}

func chooseNativePath(_ process: Process, path: URL, saving: Bool) throws {
    let confirm = saving ? ["Save", "存储", "保存"] : ["Open", "打开", "选择账号备份"]
    NSRunningApplication(processIdentifier: process.processIdentifier)?.activate(options: [])
    let button = try waitFor("macOS file panel") {
        find(process, role: "AXButton", matching: { confirm.contains($0) })
    }
    if saving {
        let filename = try waitFor("save panel with expected backup name") {
            find(process, role: "AXTextField", matching: { $0 == "Mesh Forum.mesh-account" })
        }
        try clickNative(filename)
        nativeKey(36)
        Thread.sleep(forTimeInterval: 0.6)
        if let current = find(process, role: "AXButton", matching: { confirm.contains($0) }) {
            try clickNative(current)
        }
        return
    }
    // Activate the panel before sending the Go to Folder shortcut.
    try clickNative(button)
    Thread.sleep(forTimeInterval: 0.2)
    nativeKey(5, flags: [.maskCommand, .maskShift])
    Thread.sleep(forTimeInterval: 0.35)
    nativeKey(0, flags: .maskCommand)
    nativeKey(0, text: path.path)
    Thread.sleep(forTimeInterval: 0.5)
    nativeKey(36)
    Thread.sleep(forTimeInterval: 0.55)
    if let button = find(process, role: "AXButton", matching: { confirm.contains($0) }) {
        try clickNative(button)
    } else {
        nativeKey(36)
    }
}

func enterPinyin(_ process: Process) throws -> String {
    let sources = TISCreateInputSourceList(nil, false).takeRetainedValue() as! [TISInputSource]
    guard let pinyin = sources.first(where: { source in
        guard let raw = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) else { return false }
        return (Unmanaged<CFString>.fromOpaque(raw).takeUnretainedValue() as String) == "com.apple.inputmethod.SCIM.ITABC"
    }) else { throw SmokeError(description: "Enable the macOS Pinyin - Simplified input source first.") }
    let previous = TISCopyCurrentKeyboardInputSource().takeRetainedValue()
    defer { TISSelectInputSource(previous) }
    let field = try waitFor("title input") {
        find(process, role: "AXTextField", matching: { $0 == "帖子标题" })
    }
    NSRunningApplication(processIdentifier: process.processIdentifier)?.activate(options: [])
    Thread.sleep(forTimeInterval: 0.3)
    let _ = AXUIElementSetAttributeValue(field, kAXFocusedAttribute as CFString, kCFBooleanTrue)
    guard TISSelectInputSource(pinyin) == noErr else {
        throw SmokeError(description: "Could not select the macOS Pinyin input source")
    }
    Thread.sleep(forTimeInterval: 0.4)
    for code: CGKeyCode in [45, 34, 4, 0, 31, 49, 36] {
        for down in [true, false] {
            CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)?.post(tap: .cghidEventTap)
        }
        Thread.sleep(forTimeInterval: 0.12)
    }
    let title = "你好"
    _ = try waitFor("Pinyin composition committed as Chinese") {
        find(process, role: "AXTextField", matching: { $0 == title })
    }
    guard find(process, role: "AXButton", matching: { $0 == "发布" }) != nil else {
        throw SmokeError(description: "Return published the draft before clicking Publish")
    }
    return title
}

func launch(_ executable: URL, _ root: URL, _ name: String, _ discoveryPort: Int) throws -> Process {
    let process = Process()
    process.executableURL = executable
    process.environment = ProcessInfo.processInfo.environment.merging([
        "MESH_FORUM_DATA": root.appendingPathComponent(name).path,
        "MESH_FORUM_TEST_CHINESE": "1",
        "MESH_FORUM_LISTEN": "0.0.0.0:0",
        "MESH_FORUM_DISCOVERY_PORT": String(discoveryPort),
    ]) { _, override in override }
    if ProcessInfo.processInfo.environment["MESH_FORUM_TEST_RECOVERY"] == "1" {
        process.environment?["MESH_FORUM_TEST_BACKUP_DIR"] = root.path
        if ProcessInfo.processInfo.environment["MESH_FORUM_TEST_NATIVE_PANEL"] != "1" {
            process.environment?["MESH_FORUM_TEST_BACKUP_PATH"] = root.appendingPathComponent("Mesh Forum.mesh-account").path
        }
    }
    if background && ProcessInfo.processInfo.environment["MESH_FORUM_TEST_RECOVERY"] != "1" {
        process.environment?["MESH_FORUM_TEST_ATTACHMENT_PATH"] = root.appendingPathComponent("fixture.txt").path
        process.environment?["MESH_FORUM_TEST_ATTACHMENT_DESTINATION"] = root.appendingPathComponent("downloaded.txt").path
    }
    if let captures = ProcessInfo.processInfo.environment["MESH_FORUM_CAPTURE_DIR"] {
        process.environment?["MESH_FORUM_CAPTURE_DIR"] = URL(fileURLWithPath: captures).appendingPathComponent(name).path
    }
    try process.run()
    return process
}

func smoke() throws {
    guard AXIsProcessTrusted() else {
        throw SmokeError(description: "Grant Accessibility access to the process that runs Swift.")
    }
    let executable = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "target/debug/mesh-forum-desktop")
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    var processes: [Process] = []
    defer {
        for process in processes where process.isRunning { process.terminate() }
        for process in processes { process.waitUntilExit() }
        try? FileManager.default.removeItem(at: root)
    }
    let discoveryPort = Int.random(in: 49000...64000)
    let a = try launch(executable, root, "a", discoveryPort)
    processes.append(a)
    try press(a, "节点")
    let b = try launch(executable, root, "b", discoveryPort)
    processes.append(b)
    try press(b, "节点")
    _ = try waitFor("automatic discovery of A") {
        find(b, role: "AXStaticText", matching: { $0.contains("最近检测可达") })
    }

    try press(a, "写帖子")
    let title: String
    if ProcessInfo.processInfo.environment["MESH_FORUM_TEST_IME"] == "1" {
        title = try enterPinyin(a)
    } else {
        title = "离线书签验收"
        try enter(a, "帖子标题", title)
    }
    let attachment = Data("离线附件验收".utf8)
    if background {
        try attachment.write(to: root.appendingPathComponent("fixture.txt"))
        try press(a, "添加附件")
        _ = try waitFor("attachment selected") { find(a, role: "AXButton", matching: { $0 == "fixture.txt" }) }
    }
    let body = "作者离线后仍然可以阅读。"
    try enter(a, "写下你的帖子", body, multiline: true)
    try press(a, "预览")
    _ = try waitFor("preview replaces editor") {
        find(a, role: "AXTextArea", matching: { $0 == body }) == nil ? true : nil
    }
    try press(a, "撰写")
    _ = try waitFor("preview preserves draft") { find(a, role: "AXTextArea", matching: { $0 == body }) }
    try press(a, "关闭")
    _ = try waitFor("composer returns to its source") {
        find(a, role: "AXStaticText", matching: { $0 == "附近节点" })
    }
    try press(a, "写帖子")
    _ = try waitFor("closing the composer preserves the draft") {
        find(a, role: "AXTextArea", matching: { $0 == body })
    }
    try press(a, "发布")
    try press(a, "返回草稿")
    _ = try waitFor("leaving account setup preserves the draft") {
        find(a, role: "AXTextArea", matching: { $0 == body })
    }
    try press(a, "发布")
    try enter(a, "显示名称", "林")
    try enter(a, "密码", "forum-test-password", secure: true)
    try press(a, "创建账号")
    _ = try waitFor("account unlock resumes draft") {
        find(a, role: "AXTextArea", matching: { $0 == body })
    }
    try press(a, "发布")

    _ = try waitFor("published locally") { find(a, role: nil, matching: { $0 == "已发布到本机" }) }

    try press(b, "版面")
    try press(b, title)
    try press(b, "帖子书签")
    try press(b, "仅本地保存")
    _ = try waitFor("original revision bookmarked") { find(b, role: nil, matching: { $0 == "已保存到本机" }) }
    try press(a, "编辑")
    let editedBody = "这是自动同步后的中文正文。"
    try enter(a, "写下你的帖子", editedBody, multiline: true)
    try press(a, "发布")
    try press(b, "查看新版本")
    try press(b, "帖子书签")
    try press(b, "保存并共享")
    _ = try waitFor("shared bookmark") { find(b, role: nil, matching: { $0 == "已保存并共享" }) }
    if background {
        try press(b, "离线保存附件")
        _ = try waitFor("attachment retained by B") { find(b, role: nil, matching: { $0 == "附件已离线保存" }) }
    }


    try press(a, "写帖子")
    _ = try waitFor("new draft after publish") {
        guard let titleInput = find(a, role: "AXTextField", matching: { $0 == "帖子标题" }),
              (attribute(titleInput, "AXValue") as? String ?? "").isEmpty else { return nil as Bool? }
        return true
    }
    a.terminate()
    a.waitUntilExit()
    try press(b, "节点")
    try press(b, "暂停上传")
    _ = try waitFor("upload paused") { find(b, role: "AXButton", matching: { $0 == "恢复上传" }) }
    let c = try launch(executable, root, "c", discoveryPort)
    processes.append(c)
    try press(c, "节点")
    _ = try waitFor("automatic discovery of paused B") {
        find(c, role: "AXStaticText", matching: { $0.contains("可达 · 已暂停上传") })
    }
    try press(c, "版面")
    _ = try waitFor("empty board while B is paused") {
        guard let refresh = find(c, role: "AXButton", matching: { $0 == "刷新" }),
              attribute(refresh, "AXEnabled") as? Bool != false
        else { return nil as AXUIElement? }
        return find(c, role: "AXStaticText", matching: { $0.hasPrefix("暂无附近帖子") })
    }
    try press(b, "恢复上传")
    _ = try waitFor("upload resumed") { find(b, role: "AXButton", matching: { $0 == "暂停上传" }) }
    try press(c, "版面")
    try press(c, title)
    do {
        _ = try waitFor("downloaded detail on C") { find(c, role: "AXStaticText", matching: { $0 == title }) }
    } catch {
        let windows = attribute(app(c), "AXWindows") as? [AXUIElement] ?? []
        print("Reader state on C:", windows.flatMap { descendants($0) }.flatMap(names))
        throw error
    }
    let readerRoot = root.appendingPathComponent("c")
    let savedBody = Data(editedBody.utf8)
    let cached = try FileManager.default.contentsOfDirectory(at: readerRoot, includingPropertiesForKeys: nil)
        .filter { $0.pathExtension == "post" }
    guard cached.count == 1,
          try Data(contentsOf: cached[0]).range(of: savedBody) != nil
    else { throw SmokeError(description: "C did not persist the expected Chinese body") }
    if background {
        let destination = root.appendingPathComponent("downloaded.txt")
        let existing = Data("existing export".utf8)
        try existing.write(to: destination)
        try press(c, "下载附件")
        _ = try waitFor("attachment conflict has an inline retry") {
            find(c, role: "AXStaticText", matching: { $0 == "保存位置已有同名文件，请另选位置。" })
        }
        guard try Data(contentsOf: destination) == existing else {
            throw SmokeError(description: "An existing file was overwritten")
        }
        try FileManager.default.removeItem(at: destination)
        try press(c, "重试")
        _ = try waitFor("attachment delivered by B") { find(c, role: nil, matching: { $0 == "附件已下载" }) }
        guard try Data(contentsOf: destination) == attachment else {
            throw SmokeError(description: "C did not receive the exact attachment bytes")
        }
        guard !processes.contains(where: { $0.processIdentifier == NSWorkspace.shared.frontmostApplication?.processIdentifier }) else {
            throw SmokeError(description: "A test instance became the foreground application")
        }
        print("PASS: C downloaded and verified the attachment from B in the background.")
    }
    if ProcessInfo.processInfo.environment["MESH_FORUM_TEST_NARROW"] != nil {
        try press(c, "返回列表")
        _ = try waitFor("narrow list restored") { find(c, role: "AXButton", matching: { $0 == title }) }
        try press(c, title)
        _ = try waitFor("narrow reader reopened") { find(c, role: "AXStaticText", matching: { $0 == title }) }
    }
    try press(c, "存储")
    _ = try waitFor("storage separates cache and offline bookmarks") {
        guard find(c, role: "AXStaticText", matching: { $0 == "自动清理" }) != nil
        else { return nil as AXUIElement? }
        return find(c, role: "AXStaticText", matching: { $0 == "离线书签" })
    }
    print("PASS: Three native desktop processes. B discovered the edit without refreshing. C downloaded the edited Chinese post from B after A exited.")
}

func recoverySmoke() throws {
    guard AXIsProcessTrusted() else {
        throw SmokeError(description: "Grant Accessibility access to the process that runs Swift.")
    }
    let executable = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "target/debug/mesh-forum-desktop")
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    var processes: [Process] = []
    defer {
        for process in processes where process.isRunning { process.terminate() }
        for process in processes { process.waitUntilExit() }
        try? FileManager.default.removeItem(at: root)
    }
    let port = Int.random(in: 49000...64000)
    let original = try launch(executable, root, "original", port)
    processes.append(original)
    try press(original, "账号")
    try enter(original, "显示名称", "林")
    try enter(original, "密码", "forum-test-password", secure: true)
    try press(original, "创建账号")
    try press(original, "备份账号")
    try press(original, "选择保存位置")
    let backup = root.appendingPathComponent("Mesh Forum.mesh-account")
    let nativePanel = ProcessInfo.processInfo.environment["MESH_FORUM_TEST_NATIVE_PANEL"] == "1"
    if nativePanel {
        try chooseNativePath(original, path: root, saving: true)
    }
    do {
        _ = try waitFor("encrypted backup exported in the native panel") {
            FileManager.default.fileExists(atPath: backup.path) ? true : nil
        }
    } catch {
        let windows = attribute(app(original), "AXWindows") as? [AXUIElement] ?? []
        let visible = windows.flatMap { descendants($0) }.flatMap(names)
        print("Account test profile files:", (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? [])
        print("Save panel still open:", visible.contains("Save") || visible.contains("存储"))
        print("Backup completed:", visible.contains("加密账号备份已保存"))
        throw error
    }
    try press(original, "写帖子")
    let title = "恢复账号验收"
    try enter(original, "帖子标题", title)
    try enter(original, "写下你的帖子", "原作者发布", multiline: true)
    try press(original, "发布")
    _ = try waitFor("original published") { find(original, role: "AXStaticText", matching: { $0 == "已发布到本机" }) }

    let restored = try launch(executable, root, "restored", port)
    processes.append(restored)
    try press(restored, "账号")
    try press(restored, "从备份恢复")
    try press(restored, "选择备份文件")
    if nativePanel {
        try chooseNativePath(restored, path: backup, saving: false)
    }
    _ = try waitFor("selected backup shown") {
        find(restored, role: "AXStaticText", matching: { $0 == backup.lastPathComponent })
    }
    try enter(restored, "密码", "incorrect-password", secure: true)
    try press(restored, "恢复账号")
    _ = try waitFor("wrong password is shown inline") {
        find(restored, role: "AXStaticText", matching: { $0.contains("密码错误") })
    }
    try enter(restored, "密码", "forum-test-password", secure: true)
    try press(restored, "恢复账号")
    _ = try waitFor("restored account") { find(restored, role: "AXButton", matching: { $0 == "退出登录" }) }
    try press(restored, "版面")
    try press(restored, title)
    try press(restored, "编辑")
    try enter(restored, "写下你的帖子", "恢复账号后继续编辑同一篇帖子。", multiline: true)
    try press(restored, "发布")
    _ = try waitFor("restored account signed a new revision") {
        find(restored, role: "AXStaticText", matching: { $0 == "已发布到本机" })
    }
    print("PASS: Account transfer using \(nativePanel ? "native panels" : "fixture picker paths"). Wrong-password retry worked and the restored account edited the original post.")
}

do {
    if background && ["MESH_FORUM_TEST_NATIVE_PANEL", "MESH_FORUM_TEST_IME"].contains(where: {
        ProcessInfo.processInfo.environment[$0] == "1"
    }) {
        throw SmokeError(description: "Native file panels and input-method tests require a dedicated foreground session.")
    }
    guard background || ProcessInfo.processInfo.environment["MESH_FORUM_ALLOW_FOREGROUND_TESTS"] == "1" else {
        throw SmokeError(description: "This smoke test activates windows. Run it in a dedicated macOS session with MESH_FORUM_ALLOW_FOREGROUND_TESTS=1.")
    }
    if ProcessInfo.processInfo.environment["MESH_FORUM_TEST_RECOVERY"] == "1" {
        try recoverySmoke()
    } else {
        try smoke()
    }
} catch {
    fputs("FAIL: \(error)\n", stderr)
    exit(1)
}
