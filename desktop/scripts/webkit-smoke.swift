// macOS WebKit keyboard harness (local #247, grown from the #242 tab-order probe). It loads a page in a WKWebView, the
// engine Tauri uses on macOS, and for each step of a JSON plan runs the step's setup JavaScript, then sends real Tab or
// Shift-Tab key events through NSWindow (the path a keyboard takes, not a DOM event) and after each press runs the step's
// probe JavaScript. It judges nothing: scripts/smoke-webkit.mjs writes the plan, serves the build on the smoke's fake
// daemon and bridge, and asserts on what this prints.
//
//   swiftc -o <bin> desktop/scripts/webkit-smoke.swift && <bin> <plan.json> [-AppleKeyboardUIMode 0]
//   swift desktop/scripts/webkit-smoke.swift <plan.json>          (interpreted; slower to start)
//
// Plan: { "url": "...", "tabFocusesLinks": false, "userScript": "js run at document start",
//         "ready": "js expression, true once the app booted", "steps": [{ "id": "...", "setup": "js (may return a promise)", "probe": "js returning a JSON string with
//         { focus, done }", "max": 40, "shift": false, "settleMs": 300 }] }
// `tabFocusesLinks` sets WKPreferences.tabFocusesLinks the way wry 0.57 does for the shipped app
// (wry-0.57.0/src/wkwebview/mod.rs:517, setValue_forKey(YES, "tabFocusesLinks")): with it on, Tab reaches buttons and
// links whatever macOS "Keyboard navigation" says. smoke-webkit.mjs runs the plan twice, on (the shipped configuration)
// and off (WebKit's default, so the tabindex stamps of #242 stay proven). The key is set by KVC, as wry does, so a
// WebKit that drops it raises here instead of silently checking the wrong thing; the value read back is in the result.
// Prints one line `WEBKIT-SMOKE-RESULT {json}` with the WebKit version, AppKit's fullKeyboardAccess flag (advisory: it
// read false even with the mode forced on; the smoke's control step is the evidence) and per step the setup error, the
// probe answers (one per press) and whether the probe said done.
//
// `-AppleKeyboardUIMode 0` (the argument domain) runs this process with macOS "Keyboard navigation" off, the default,
// whatever the host's System Settings say (`2` forces it on: the smoke's control step then fails, as it should).
// The app is an accessory that never activates: the window shows for the run but the operator's keyboard focus stays
// where it was, because the key events go straight to the window (NSWindow.sendEvent), not through the system.
// The page runs on WKWebsiteDataStore.nonPersistent(), so no website data is written and every run starts clean (#263).
// Exit 0 the plan ran (even when a step's probe never said done), 2 bad usage or plan, 3 runner_unavailable (no window
// server session: an ssh login or a daemon outside the GUI session), 4 the page never booted, 5 the run timed out.
import Cocoa
import CoreGraphics
import WebKit

struct Step: Decodable {
  let id: String
  let setup: String
  let probe: String
  let max: Int
  let shift: Bool?
  let settleMs: Int?
}

struct Plan: Decodable {
  let url: String
  let tabFocusesLinks: Bool?
  let userScript: String?
  let ready: String
  let steps: [Step]
  let timeoutSeconds: Double?
}

struct StepResult: Encodable {
  let id: String
  var setupError: String?
  var presses: [String] = []
  var done = false
}

struct RunResult: Encodable {
  var tabFocusesLinks: Bool?
  var fullKeyboardAccess = false
  var webkitVersion = ""
  var booted = false
  var timedOut = false
  var steps: [StepResult] = []
}

func fail(_ code: Int32, _ message: String) -> Never {
  FileHandle.standardError.write("\(message)\n".data(using: .utf8)!)
  exit(code)
}

let args = CommandLine.arguments
guard args.count >= 2, !args[1].hasPrefix("-") else { fail(2, "usage: webkit-smoke <plan.json> [-AppleKeyboardUIMode 0]") }
guard let planData = FileManager.default.contents(atPath: args[1]) else { fail(2, "cannot read plan \(args[1])") }
let plan: Plan
do { plan = try JSONDecoder().decode(Plan.self, from: planData) } catch { fail(2, "bad plan: \(error)") }
guard let url = URL(string: plan.url) else { fail(2, "bad plan url \(plan.url)") }
// No window server session (ssh, a LaunchDaemon, a CI job outside the GUI login): NSApplication would abort.
if CGSessionCopyCurrentDictionary() == nil {
  print("runner_unavailable: no window server session (CGSessionCopyCurrentDictionary is nil); 0 steps executed")
  exit(3)
}

final class Harness: NSObject, NSApplicationDelegate, WKNavigationDelegate {
  let plan: Plan
  let url: URL
  var win: NSWindow!
  var web: WKWebView!
  var result = RunResult()
  var loaded = false

  init(plan: Plan, url: URL) { self.plan = plan; self.url = url }

  func applicationDidFinishLaunching(_ n: Notification) {
    let config = WKWebViewConfiguration()
    // Nothing of this run reaches the disk and nothing carries over from an earlier one (#263): a non-persistent store
    // keeps cookies, localStorage, IndexedDB and caches in memory. (A bundle-less binary otherwise gets
    // ~/Library/WebKit/<name> and ~/Library/Caches/<name>.) smoke-webkit.mjs also runs this process with a temp HOME.
    config.websiteDataStore = WKWebsiteDataStore.nonPersistent()
    if let links = plan.tabFocusesLinks {
      config.preferences.setValue(links, forKey: "tabFocusesLinks")
      result.tabFocusesLinks = (config.preferences.value(forKey: "tabFocusesLinks") as? NSNumber)?.boolValue
    }
    if let source = plan.userScript, !source.isEmpty {
      config.userContentController.addUserScript(WKUserScript(source: source, injectionTime: .atDocumentStart, forMainFrameOnly: true))
    }
    win = NSWindow(contentRect: NSRect(x: 40, y: 40, width: 1280, height: 800), styleMask: [.titled], backing: .buffered, defer: false)
    win.title = "rsi WebKit smoke"
    web = WKWebView(frame: win.contentView!.bounds, configuration: config)
    web.autoresizingMask = [.width, .height]
    web.navigationDelegate = self
    win.contentView!.addSubview(web)
    win.orderFrontRegardless()
    win.makeKey()
    result.fullKeyboardAccess = NSApp.isFullKeyboardAccessEnabled
    result.webkitVersion = Bundle(for: WKWebView.self).object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? ""
    web.load(URLRequest(url: url))
    DispatchQueue.main.asyncAfter(deadline: .now() + (plan.timeoutSeconds ?? 50)) {
      self.result.timedOut = true
      self.finish(5)
    }
  }

  func webView(_ w: WKWebView, didFinish n: WKNavigation!) {
    if loaded { return }
    loaded = true
    waitReady(deadline: Date().addingTimeInterval(15))
  }

  func webView(_ w: WKWebView, didFail n: WKNavigation!, withError e: Error) { finish(4, "navigation failed: \(e)") }
  func webView(_ w: WKWebView, didFailProvisionalNavigation n: WKNavigation!, withError e: Error) { finish(4, "navigation failed: \(e)") }

  func waitReady(deadline: Date) {
    web.evaluateJavaScript("!!(\(plan.ready))") { r, _ in
      if (r as? Bool) == true {
        self.result.booted = true
        self.win.makeFirstResponder(self.web)
        self.runStep(0)
      } else if Date() > deadline {
        self.finish(4, "the page never became ready: \(self.plan.ready)")
      } else {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { self.waitReady(deadline: deadline) }
      }
    }
  }

  func runStep(_ i: Int) {
    if i >= plan.steps.count { finish(0); return }
    let step = plan.steps[i]
    result.steps.append(StepResult(id: step.id))
    web.callAsyncJavaScript("return await (\(step.setup));", arguments: [:], in: nil, in: .page) { r in
      if case .failure(let e) = r { self.result.steps[i].setupError = "\(e)" }
      DispatchQueue.main.asyncAfter(deadline: .now() + Double(step.settleMs ?? 300) / 1000) {
        self.win.makeKey()
        self.win.makeFirstResponder(self.web)
        self.press(i, 0)
      }
    }
  }

  func press(_ i: Int, _ n: Int) {
    let step = plan.steps[i]
    if n >= step.max { runStep(i + 1); return }
    let shift = step.shift ?? false
    // Shift-Tab arrives as the back-tab character, as AppKit delivers it from a real keyboard.
    let chars = shift ? "\u{19}" : "\t"
    for type in [NSEvent.EventType.keyDown, .keyUp] {
      let ev = NSEvent.keyEvent(with: type, location: .zero, modifierFlags: shift ? [.shift] : [], timestamp: ProcessInfo.processInfo.systemUptime,
                                windowNumber: win.windowNumber, context: nil, characters: chars, charactersIgnoringModifiers: chars,
                                isARepeat: false, keyCode: 48)!
      win.sendEvent(ev)
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
      self.web.evaluateJavaScript("String(\(step.probe))") { r, err in
        let answer = (r as? String) ?? "{\"error\":\(Self.quote(err.map { "\($0)" } ?? "no answer"))}"
        self.result.steps[i].presses.append(answer)
        if let d = answer.data(using: .utf8), let o = try? JSONSerialization.jsonObject(with: d) as? [String: Any], (o["done"] as? Bool) == true {
          self.result.steps[i].done = true
          self.runStep(i + 1)
        } else {
          self.press(i, n + 1)
        }
      }
    }
  }

  static func quote(_ s: String) -> String {
    let d = try! JSONSerialization.data(withJSONObject: [s])
    let a = String(data: d, encoding: .utf8)!
    return String(a.dropFirst().dropLast())
  }

  var finished = false
  func finish(_ code: Int32, _ message: String? = nil) {
    if finished { return }
    finished = true
    if let message { FileHandle.standardError.write("\(message)\n".data(using: .utf8)!) }
    let enc = JSONEncoder()
    enc.outputFormatting = [.sortedKeys]
    let json = String(data: try! enc.encode(result), encoding: .utf8)!
    print("WEBKIT-SMOKE-RESULT \(json)")
    fflush(stdout)
    exit(code)
  }
}

let app = NSApplication.shared
let harness = Harness(plan: plan, url: url)
app.delegate = harness
app.setActivationPolicy(.accessory)
app.run()
