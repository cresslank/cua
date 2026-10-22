import AppKit

class ReceiverWindow: NSWindow {
    var count = 0
    var nestedCount = 0
    @objc func countAction(_ sender: Any?) { count += 1; receiver.event("count", self); receiver.writeState() }
    @objc func nestedAction(_ sender: Any?) { nestedCount += 1; receiver.event("nested", self); receiver.writeState() }
}

final class TargetWindow: ReceiverWindow {
    @objc func targetOnlyAction(_ sender: Any?) {
        count += 1
        receiver.event("target-only", self)
        receiver.writeState()
    }
}

final class Receiver: NSObject, NSApplicationDelegate, NSMenuDelegate {
    let root = URL(fileURLWithPath: ProcessInfo.processInfo.environment["CUA_MENU_RECEIVER_ROOT"]!)
    var windows: [ReceiverWindow] = []
    var opens = 0
    var closes = 0
    var timer: Timer?
    var events: [[String: Any]] = []
    func event(_ name: String, _ window: ReceiverWindow? = nil) {
        events.append(["event": name, "window": window?.windowNumber ?? -1,
                       "key": NSApp.keyWindow?.windowNumber ?? -1, "active": NSApp.isActive])
    }
    func applicationDidFinishLaunching(_ note: Notification) {
        let menu = NSMenu()
        let appItem = NSMenuItem(title: "MenuReceiver", action: nil, keyEquivalent: "")
        let appMenu = NSMenu(title: "MenuReceiver")
        appItem.submenu = appMenu
        menu.addItem(appItem)
        let testItem = NSMenuItem(title: "Probe", action: nil, keyEquivalent: "")
        let testMenu = NSMenu(title: "Probe")
        testMenu.delegate = self
        testMenu.addItem(withTitle: "Count", action: #selector(ReceiverWindow.countAction(_:)), keyEquivalent: "")
        let nested = NSMenuItem(title: "Nested", action: nil, keyEquivalent: "")
        let nestedMenu = NSMenu(title: "Nested")
        nestedMenu.delegate = self
        nestedMenu.addItem(withTitle: "Count Nested", action: #selector(ReceiverWindow.nestedAction(_:)), keyEquivalent: "")
        // Only window 1 implements this responder-chain command. Restoring
        // window 0 before AppKit dispatches it silently drops the action.
        nestedMenu.addItem(withTitle: "Target Only", action: #selector(TargetWindow.targetOnlyAction(_:)), keyEquivalent: "")
        nested.submenu = nestedMenu
        testMenu.addItem(nested)
        let disabled = NSMenuItem(title: "Disabled", action: nil, keyEquivalent: "")
        disabled.isEnabled = false
        testMenu.addItem(disabled)
        testItem.submenu = testMenu
        menu.addItem(testItem)
        NSApp.mainMenu = menu
        for i in 0..<2 {
            let frame = NSRect(x: 40 + i * 380, y: 100, width: 360, height: 160)
            let w: ReceiverWindow = i == 1
                ? TargetWindow(contentRect: frame, styleMask: [.titled, .closable], backing: .buffered, defer: false)
                : ReceiverWindow(contentRect: frame, styleMask: [.titled, .closable], backing: .buffered, defer: false)
            w.title = "Owned Menu Receiver \(i)"
            w.isReleasedWhenClosed = false
            let label = NSTextField(labelWithString: "Throwaway Cua menu receiver \(i)")
            label.frame = NSRect(x: 16, y: 70, width: 320, height: 24)
            w.contentView?.addSubview(label)
            w.orderBack(nil)
            windows.append(w)
            for name in [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification] {
                NotificationCenter.default.addObserver(forName: name, object: w, queue: .main) { n in
                    self.event(n.name.rawValue, w)
                }
            }
        }
        // Establish a receiver-local key window without application activation.
        windows[0].makeKey()
        timer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { [weak self] _ in
            guard let self else { return }
            let commandURL = self.root.appendingPathComponent("command")
            if let command = try? String(contentsOf: commandURL, encoding: .utf8) {
                try? FileManager.default.removeItem(at: commandURL)
                switch command.trimmingCharacters(in: .whitespacesAndNewlines) {
                case "cancel": NSApp.mainMenu?.cancelTracking()
                case "quit": NSApp.terminate(nil)
                default: break
                }
            }
            self.writeState()
        }
        writeState()
    }
    func menuWillOpen(_ menu: NSMenu) { opens += 1; writeState() }
    func menuDidClose(_ menu: NSMenu) { closes += 1; writeState() }
    func writeState() {
        let data: [String: Any] = ["pid": ProcessInfo.processInfo.processIdentifier,
            "active": NSApp.isActive, "opens": opens, "closes": closes, "events": events,
            "front_pid": NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1,
            "windows": windows.map { ["id": $0.windowNumber, "title": $0.title,
                                      "count": $0.count, "nested_count": $0.nestedCount,
                                      "key": $0.isKeyWindow, "main": $0.isMainWindow] }]
        if let json = try? JSONSerialization.data(withJSONObject: data, options: [.sortedKeys]) {
            try? json.write(to: root.appendingPathComponent("state.json"), options: .atomic)
        }
    }
}
let app = NSApplication.shared
let receiver = Receiver()
app.setActivationPolicy(.regular)
app.delegate = receiver
app.run()
