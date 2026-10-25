// In-process driver for the packaged RSI Desktop app (local #259, committed from the #255 acceptance run).
//
// scripts/smoke-app.mjs compiles this into a dylib and loads it with DYLD_INSERT_LIBRARIES into a COPY of the app that
// it launched itself, under a test bundle id. The agent has no macOS privacy (TCC) grant: no Accessibility, no event
// posting, no screen capture. Inside the app's own process none is needed:
//   - key events go to the app's own window through -[NSWindow sendEvent:] (the path a keyboard takes into AppKit; the
//     system's event stream and the operator's keyboard focus are never touched);
//   - page JavaScript runs with -[WKWebView callAsyncJavaScript:...];
//   - snapshots come from -[WKWebView takeSnapshotWithConfiguration:] (no Screen Recording grant);
//   - the app's own window-level AX tree is readable through AXUIElementCreateApplication(getpid()).
// It refuses to act unless RSI_APP_DRIVER_CTL is set AND the bundle id starts with dev.rsi.desktop.smoke-app, so it can
// never drive the operator's installed app (dev.rsi.desktop) even if loaded there by mistake.
//
// Protocol: the caller writes $RSI_APP_DRIVER_CTL/cmd.json atomically ({"id": n, "op": "...", ...}); the driver polls it
// every 25 ms on the main thread, runs a new id once and writes resp-<id>.json atomically. Ops:
//   info                               pid, bundle id, windows, WebKit version, keyboard-navigation facts
//   deactivate                         accessory policy, never key app: the operator's focus stays where it is
//   js {js}                            the body of an async function; its return value is JSON-encoded
//   key {keys: [name...]}              tab, shift-tab, f6, shift-f6, esc, enter
//   type {text}                        characters as real key events (lowercase, digits, space, a few symbols, \r)
//   walk {setup, probe, key, max, settleMs}  #247's step loop: await setup, then press `key` up to max times, running
//                                      `probe` (an expression returning a JSON string with `done`) after each press
//   snap {path}                        PNG of the web view
//   ax {path, depth}                   the app's AX tree (window level; the menu bar is skipped, it lists recent items)
//   quit                               -[NSApp terminate:]
// The constructor sets AppleKeyboardUIMode=0 in the volatile argument domain (this process only: macOS "Keyboard
// navigation" off, nothing written to disk) and unsets DYLD_INSERT_LIBRARIES so the shells the terminal starts do not
// load the driver.
//
//   xcrun clang -dynamiclib -fobjc-arc -framework Cocoa -framework WebKit -framework ApplicationServices \
//     -o app-driver.dylib desktop/scripts/app-driver.m
#import <Cocoa/Cocoa.h>
#import <WebKit/WebKit.h>
#import <ApplicationServices/ApplicationServices.h>

static NSString *const kBundlePrefix = @"dev.rsi.desktop.smoke-app";
static NSString *ctl;
static long lastId = -1;

static WKWebView *findWeb(NSView *v) {
  if ([v isKindOfClass:[WKWebView class]]) return (WKWebView *)v;
  for (NSView *s in v.subviews) { WKWebView *w = findWeb(s); if (w) return w; }
  return nil;
}

static NSWindow *appWindow(WKWebView **outWeb) {
  for (NSWindow *w in NSApp.windows) {
    WKWebView *web = w.contentView ? findWeb(w.contentView) : nil;
    if (web) { if (outWeb) *outWeb = web; return w; }
  }
  return nil;
}

static void respond(long cid, NSDictionary *body) {
  NSMutableDictionary *d = [body mutableCopy];
  d[@"id"] = @(cid);
  NSData *data = [NSJSONSerialization dataWithJSONObject:d options:NSJSONWritingSortedKeys error:nil];
  NSString *tmp = [ctl stringByAppendingFormat:@"/.resp-%ld.tmp", cid];
  NSString *dst = [ctl stringByAppendingFormat:@"/resp-%ld.json", cid];
  [data writeToFile:tmp atomically:NO];
  rename(tmp.UTF8String, dst.UTF8String);
}

// ANSI virtual key codes for the characters the terminal check types (index = key code; '?' marks a gap).
static unsigned short keyCodeFor(unichar c, BOOL *shift) {
  *shift = NO;
  const char *keys = "asdfhgzxcv?bqweryt123465=97-80]ou[ip?lj'k;\\,/nm.";
  if (c >= 'A' && c <= 'Z') { *shift = YES; c = c - 'A' + 'a'; }
  if (c == ':') { *shift = YES; return 41; }
  if (c == '!') { *shift = YES; return 18; }
  if (c == '_') { *shift = YES; return 27; }
  if (c == ' ') return 49;
  for (unsigned short i = 0; keys[i]; i++) if (keys[i] == c && c != '?') return i;
  return 0xFFFF;
}

static void sendKey(NSWindow *win, unsigned short code, NSString *chars, NSString *ignoring, NSEventModifierFlags flags) {
  for (NSNumber *t in @[@(NSEventTypeKeyDown), @(NSEventTypeKeyUp)]) {
    NSEvent *ev = [NSEvent keyEventWithType:(NSEventType)t.unsignedIntegerValue location:NSZeroPoint modifierFlags:flags
                                  timestamp:[NSProcessInfo processInfo].systemUptime windowNumber:win.windowNumber context:nil
                                 characters:chars charactersIgnoringModifiers:ignoring isARepeat:NO keyCode:code];
    [win sendEvent:ev];
  }
}

static void focusWeb(NSWindow *win, WKWebView *web) {
  [win makeKeyWindow];
  if (win.firstResponder == win || win.firstResponder == nil) [win makeFirstResponder:web];
}

static NSString *pressNamed(NSWindow *win, WKWebView *web, NSString *name) {
  focusWeb(win, web);
  unichar f6 = NSF6FunctionKey;
  NSString *f6s = [NSString stringWithCharacters:&f6 length:1];
  // Shift-Tab arrives as the back-tab character, as AppKit delivers it from a real keyboard.
  if ([name isEqual:@"tab"]) sendKey(win, 48, @"\t", @"\t", 0);
  else if ([name isEqual:@"shift-tab"]) sendKey(win, 48, @"\x19", @"\x19", NSEventModifierFlagShift);
  else if ([name isEqual:@"f6"]) sendKey(win, 97, f6s, f6s, NSEventModifierFlagFunction);
  else if ([name isEqual:@"shift-f6"]) sendKey(win, 97, f6s, f6s, NSEventModifierFlagFunction | NSEventModifierFlagShift);
  else if ([name isEqual:@"esc"]) sendKey(win, 53, @"\x1b", @"\x1b", 0);
  else if ([name isEqual:@"enter"]) sendKey(win, 36, @"\r", @"\r", 0);
  else return [@"unknown key " stringByAppendingString:name];
  return nil;
}

static NSString *driverTypeText(NSWindow *win, WKWebView *web, NSString *text) {
  focusWeb(win, web);
  for (NSUInteger i = 0; i < text.length; i++) {
    unichar c = [text characterAtIndex:i];
    if (c == '\r' || c == '\n') { sendKey(win, 36, @"\r", @"\r", 0); continue; }
    BOOL shift;
    unsigned short code = keyCodeFor(c, &shift);
    if (code == 0xFFFF) return [NSString stringWithFormat:@"cannot type character U+%04X", c];
    NSString *s = [NSString stringWithCharacters:&c length:1];
    unichar lower = (c >= 'A' && c <= 'Z') ? c - 'A' + 'a' : c;
    sendKey(win, code, s, [NSString stringWithCharacters:&lower length:1], shift ? NSEventModifierFlagShift : 0);
  }
  return nil;
}

static id axValue(AXUIElementRef e, CFStringRef attr) {
  CFTypeRef v = NULL;
  if (AXUIElementCopyAttributeValue(e, attr, &v) != kAXErrorSuccess || !v) return nil;
  return CFBridgingRelease(v);
}

static NSDictionary *axNode(AXUIElementRef e, int depth, int *budget) {
  NSMutableDictionary *n = [NSMutableDictionary dictionary];
  for (NSString *a in @[@"AXRole", @"AXSubrole", @"AXRoleDescription", @"AXTitle", @"AXDescription", @"AXIdentifier", @"AXFocused"]) {
    id v = axValue(e, (__bridge CFStringRef)a);
    if (v && ([v isKindOfClass:[NSString class]] || [v isKindOfClass:[NSNumber class]])) {
      if ([v isKindOfClass:[NSString class]] && [v length] > 160) v = [[v substringToIndex:160] stringByAppendingString:@"…"];
      n[a] = v;
    }
  }
  if (depth > 0 && *budget > 0) {
    NSArray *kids = axValue(e, kAXChildrenAttribute);
    NSMutableArray *out = [NSMutableArray array];
    for (id k in kids) {
      if (--(*budget) <= 0) break;
      // The system Apple menu lists the operator's recent items: never dumped.
      if ([axValue((__bridge AXUIElementRef)k, kAXRoleAttribute) isEqual:@"AXMenuBar"]) continue;
      [out addObject:axNode((__bridge AXUIElementRef)k, depth - 1, budget)];
    }
    if (out.count) n[@"children"] = out;
  }
  return n;
}

// #247's step loop (scripts/webkit-smoke.swift), run inside the app: one key, then the probe, until done or max.
static void walkPress(long cid, NSWindow *win, WKWebView *web, NSDictionary *cmd, NSMutableArray *presses) {
  NSInteger max = [cmd[@"max"] integerValue];
  if ((NSInteger)presses.count >= max) { respond(cid, @{ @"ok": @YES, @"presses": presses, @"done": @NO }); return; }
  NSString *err = pressNamed(win, web, cmd[@"key"] ?: @"tab");
  if (err) { respond(cid, @{ @"ok": @NO, @"error": err, @"presses": presses }); return; }
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
    [web evaluateJavaScript:[NSString stringWithFormat:@"String(%@)", cmd[@"probe"]] completionHandler:^(id r, NSError *e) {
      NSString *answer = [r isKindOfClass:[NSString class]] ? r : nil;
      if (!answer) {
        NSData *d = [NSJSONSerialization dataWithJSONObject:@{ @"error": e.localizedDescription ?: @"no answer" } options:0 error:nil];
        answer = [[NSString alloc] initWithData:d encoding:NSUTF8StringEncoding];
      }
      [presses addObject:answer];
      NSDictionary *o = [NSJSONSerialization JSONObjectWithData:[answer dataUsingEncoding:NSUTF8StringEncoding] options:0 error:nil];
      if ([o isKindOfClass:[NSDictionary class]] && [o[@"done"] isEqual:@YES]) respond(cid, @{ @"ok": @YES, @"presses": presses, @"done": @YES });
      else walkPress(cid, win, web, cmd, presses);
    }];
  });
}

static void handle(NSDictionary *cmd) {
  long cid = [cmd[@"id"] longValue];
  NSString *op = cmd[@"op"];
  WKWebView *web = nil;
  NSWindow *win = appWindow(&web);
  if ([op isEqual:@"info"]) {
    NSMutableArray *wins = [NSMutableArray array];
    for (NSWindow *w in NSApp.windows) [wins addObject:@{ @"title": w.title ?: @"", @"visible": @(w.isVisible), @"key": @(w.isKeyWindow) }];
    id mode = [[NSUserDefaults standardUserDefaults] objectForKey:@"AppleKeyboardUIMode"];
    id links = web ? [web.configuration.preferences valueForKey:@"tabFocusesLinks"] : nil;
    respond(cid, @{ @"ok": @YES, @"pid": @(getpid()), @"bundle": [NSBundle mainBundle].bundleIdentifier ?: @"",
                    @"bundlePath": [NSBundle mainBundle].bundlePath ?: @"", @"windows": wins, @"hasWeb": @(web != nil),
                    @"appleKeyboardUIMode": mode ?: [NSNull null], @"fullKeyboardAccess": @(NSApp.isFullKeyboardAccessEnabled),
                    @"tabFocusesLinks": links ?: [NSNull null], @"active": @(NSApp.isActive), @"axTrusted": @(AXIsProcessTrusted()),
                    @"webkit": [[NSBundle bundleForClass:[WKWebView class]] objectForInfoDictionaryKey:@"CFBundleVersion"] ?: @"",
                    @"dyldInsertInherited": @(getenv("DYLD_INSERT_LIBRARIES") != NULL) });
    return;
  }
  if ([op isEqual:@"quit"]) {
    respond(cid, @{ @"ok": @YES });
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 150 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{ [NSApp terminate:nil]; });
    return;
  }
  if (!win || !web) { respond(cid, @{ @"ok": @NO, @"error": @"no window with a WKWebView yet" }); return; }
  if ([op isEqual:@"deactivate"]) {
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [NSApp deactivate];
    [win orderFrontRegardless];
    respond(cid, @{ @"ok": @YES });
  } else if ([op isEqual:@"js"]) {
    NSString *src = [NSString stringWithFormat:@"const __r = await (async () => { %@ })(); return JSON.stringify(__r === undefined ? null : __r);", cmd[@"js"]];
    [web callAsyncJavaScript:src arguments:@{} inFrame:nil inContentWorld:WKContentWorld.pageWorld completionHandler:^(id r, NSError *err) {
      if (err) respond(cid, @{ @"ok": @NO, @"error": err.userInfo[@"WKJavaScriptExceptionMessage"] ?: err.localizedDescription ?: @"js error" });
      else respond(cid, @{ @"ok": @YES, @"result": r ?: [NSNull null] });
    }];
  } else if ([op isEqual:@"key"]) {
    NSMutableArray *errs = [NSMutableArray array];
    for (NSString *k in cmd[@"keys"]) { NSString *e = pressNamed(win, web, k); if (e) [errs addObject:e]; }
    respond(cid, @{ @"ok": @(errs.count == 0), @"errors": errs });
  } else if ([op isEqual:@"type"]) {
    NSString *e = driverTypeText(win, web, cmd[@"text"] ?: @"");
    respond(cid, e ? @{ @"ok": @NO, @"error": e } : @{ @"ok": @YES });
  } else if ([op isEqual:@"walk"]) {
    NSString *src = [NSString stringWithFormat:@"return await (%@);", cmd[@"setup"]];
    [web callAsyncJavaScript:src arguments:@{} inFrame:nil inContentWorld:WKContentWorld.pageWorld completionHandler:^(id r, NSError *err) {
      if (err) { respond(cid, @{ @"ok": @YES, @"setupError": err.userInfo[@"WKJavaScriptExceptionMessage"] ?: err.localizedDescription ?: @"setup error", @"presses": @[] }); return; }
      long settle = cmd[@"settleMs"] ? [cmd[@"settleMs"] longValue] : 250;
      dispatch_after(dispatch_time(DISPATCH_TIME_NOW, settle * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
        [win makeKeyWindow];
        [win makeFirstResponder:web];
        walkPress(cid, win, web, cmd, [NSMutableArray array]);
      });
    }];
  } else if ([op isEqual:@"snap"]) {
    [web takeSnapshotWithConfiguration:[WKSnapshotConfiguration new] completionHandler:^(NSImage *img, NSError *err) {
      if (!img) { respond(cid, @{ @"ok": @NO, @"error": err.localizedDescription ?: @"no image" }); return; }
      CGImageRef cg = [img CGImageForProposedRect:NULL context:nil hints:nil];
      NSData *png = [[[NSBitmapImageRep alloc] initWithCGImage:cg] representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
      BOOL ok = [png writeToFile:cmd[@"path"] atomically:YES];
      respond(cid, @{ @"ok": @(ok), @"path": cmd[@"path"], @"bytes": @(png.length) });
    }];
  } else if ([op isEqual:@"ax"]) {
    int depth = [cmd[@"depth"] intValue] ?: 6;
    int budget = 2000;
    AXUIElementRef app = AXUIElementCreateApplication(getpid());
    NSDictionary *tree = axNode(app, depth, &budget);
    CFRelease(app);
    NSData *data = [NSJSONSerialization dataWithJSONObject:tree options:NSJSONWritingPrettyPrinted | NSJSONWritingSortedKeys error:nil];
    [data writeToFile:cmd[@"path"] atomically:YES];
    respond(cid, @{ @"ok": @YES, @"path": cmd[@"path"], @"tree": tree, @"axTrusted": @(AXIsProcessTrusted()) });
  } else {
    respond(cid, @{ @"ok": @NO, @"error": [@"unknown op " stringByAppendingString:op ?: @"(none)"] });
  }
}

static void poll(void) {
  NSData *data = [NSData dataWithContentsOfFile:[ctl stringByAppendingString:@"/cmd.json"]];
  if (data) {
    NSDictionary *cmd = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    long cid = [cmd[@"id"] longValue];
    if ([cmd isKindOfClass:[NSDictionary class]] && cid != lastId) {
      lastId = cid;
      @try { handle(cmd); } @catch (NSException *e) { respond(cid, @{ @"ok": @NO, @"error": e.reason ?: @"exception" }); }
    }
  }
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{ poll(); });
}

__attribute__((constructor)) static void start(void) {
  // The shells the terminal starts must not load the driver, whatever happens next.
  unsetenv("DYLD_INSERT_LIBRARIES");
  const char *c = getenv("RSI_APP_DRIVER_CTL");
  NSString *bundle = [NSBundle mainBundle].bundleIdentifier ?: @"";
  if (!c || ![bundle hasPrefix:kBundlePrefix]) {
    fprintf(stderr, "app-driver: inactive (bundle %s; it drives only a %s copy with RSI_APP_DRIVER_CTL set)\n", bundle.UTF8String, kBundlePrefix.UTF8String);
    return;
  }
  ctl = [NSString stringWithUTF8String:c];
  unsetenv("RSI_APP_DRIVER_CTL");
  // This process only: macOS "Keyboard navigation" off (the argument domain is volatile, nothing is written to disk).
  [[NSUserDefaults standardUserDefaults] setVolatileDomain:@{ @"AppleKeyboardUIMode": @0 } forName:NSArgumentDomain];
  fprintf(stderr, "app-driver: active, control directory %s\n", c);
  dispatch_async(dispatch_get_main_queue(), ^{ poll(); });
}
