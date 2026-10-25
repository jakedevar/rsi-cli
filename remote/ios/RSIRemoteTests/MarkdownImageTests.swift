import SwiftUI
import XCTest

@testable import RSIRemote

/// Image sources, the loader's no-credentials guarantee, and zoom (#503).
final class ImagePolicyTests: XCTestCase {
    private func isBlocked(_ raw: String) -> Bool {
        if case .blocked = ImagePolicy.classify(raw) { true } else { false }
    }

    func testPublicHttpsImagesAreRemoteAndWaitForATap() {
        for raw in ["https://example.com/a.png", "https://cdn.example.co.uk/p/q.jpg?x=1", "HTTPS://Example.COM/A.PNG", "https://example.com:443/a.png", "https://xn--bcher-kva.example/a.png"] {
            guard case .remote = ImagePolicy.classify(raw) else { return XCTFail(raw) }
        }
    }

    func testEverythingElseIsRefusedWithAReason() {
        let refused = [
            "http://example.com/a.png", "ftp://example.com/a.png", "file:///etc/passwd", "javascript:alert(1)",
            "rsi://x/a.png", "//example.com/a.png", "relative/a.png", "", "   ",
            "https://user@example.com/a.png", "https://user:pass@example.com/a.png",
            "https://example.com:8443/a.png", "https://localhost/a.png", "https://127.0.0.1/a.png",
            "https://192.168.1.5/a.png", "https://10.0.0.1/a.png", "https://[::1]/a.png", "https://2130706433/a.png",
            "https://intranet/a.png", "https://printer.local/a.png", "https://hub.internal/a.png",
            "https://my-mac.tail1234.ts.net/a.png", "https://router.lan/a.png", "https://example.com/a.svg",
            "https://" + String(repeating: "a", count: 3000) + ".com/a.png",
            "data:text/html,<script>alert(1)</script>", "data:image/svg+xml;base64,PHN2Zz4=", "data:image/png,notbase64",
        ]
        for raw in refused {
            guard case .blocked(let reason) = ImagePolicy.classify(raw) else { return XCTFail("not refused: \(raw.prefix(60))") }
            XCTAssertFalse(reason.isEmpty)
        }
    }

    func testPublicNames() {
        XCTAssertTrue(ImagePolicy.isPublicName("example.com"))
        XCTAssertTrue(ImagePolicy.isPublicName("a.b.example.org."))
        for host in ["localhost", "example", "203.0.113.10", "a.ts.net", "ts.net", "x.home.arpa", "-bad.example.com", "a..com", "exa mple.com", "ex_ample.com"] {
            XCTAssertFalse(ImagePolicy.isPublicName(host), host)
        }
    }

    func testInlineImagesDecodeLocallyAndDamagedOnesAreRefused() {
        guard case .inline(let data) = ImagePolicy.classify(MarkdownFixture.sampleImageDataURI) else { return XCTFail("not inline") }
        let image = ImageDecoding.decode(data)
        XCTAssertEqual(image?.width, 96)
        XCTAssertEqual(image?.height, 54)
        XCTAssertTrue(isBlocked("data:image/png;base64,@@@@"))
        XCTAssertTrue(isBlocked("data:image/png;base64,"))
        let big = Data(count: ImagePolicy.maxInlineBytes + 1).base64EncodedString()
        XCTAssertTrue(isBlocked("data:image/png;base64," + big))
    }

    func testDecodingRefusesNonImagesAndDownsamplesBigOnes() {
        XCTAssertNil(ImageDecoding.decode(Data("not an image".utf8)))
        XCTAssertNil(ImageDecoding.decode(Data()))
        XCTAssertNil(ImageDecoding.decode(Data(repeating: 0xFF, count: 1000)))
        let wide = ImageDecoding.decode(MarkdownFixture.samplePNG(width: 5000, height: 100))
        XCTAssertNotNil(wide)
        XCTAssertLessThanOrEqual(wide?.width ?? 9999, ImageDecoding.maxPixelSize)
        XCTAssertEqual(wide?.width, ImageDecoding.maxPixelSize)
    }

    func testImageCacheKeepsAndBoundsEntries() {
        let cache = ImageCache(countLimit: 2, byteLimit: 10_000_000)
        let image = ImageDecoding.decode(MarkdownFixture.samplePNG())!
        cache.store(image, for: "a")
        XCTAssertEqual(cache.image(for: "a")?.width, 96)
        XCTAssertNil(cache.image(for: "missing"))
    }
}

// MARK: - The loader's requests

/// Answers every request from a script and records what was asked.
final class ImageStubProtocol: URLProtocol, @unchecked Sendable {
    struct Reply {
        var status = 200
        var headers: [String: String] = ["Content-Type": "image/png"]
        var body = Data()
    }
    nonisolated(unsafe) static var requests: [URLRequest] = []
    nonisolated(unsafe) static var script: (URLRequest) -> Reply = { _ in Reply() }
    private static let lock = NSLock()

    static func reset(_ script: @escaping (URLRequest) -> Reply) {
        lock.lock()
        requests = []
        Self.script = script
        lock.unlock()
    }

    static var seen: [URLRequest] {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.lock.lock()
        Self.requests.append(request)
        let reply = Self.script(request)
        Self.lock.unlock()
        let response = HTTPURLResponse(
            url: request.url!, statusCode: reply.status, httpVersion: "HTTP/1.1", headerFields: reply.headers)!
        if (300..<400).contains(reply.status), let location = reply.headers["Location"], let target = URL(string: location) {
            var next = URLRequest(url: target)
            next.allHTTPHeaderFields = request.allHTTPHeaderFields
            client?.urlProtocol(self, wasRedirectedTo: next, redirectResponse: response)
            // If the redirect is refused the task ends with this response, as with a real server.
        }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: reply.body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

final class RemoteImageLoaderTests: XCTestCase {
    private let png = MarkdownFixture.samplePNG()
    private func loader() -> RemoteImageLoader { RemoteImageLoader(protocolClasses: [ImageStubProtocol.self]) }

    func testTheRequestCarriesNothingButTheAddress() async throws {
        ImageStubProtocol.reset { _ in .init(body: self.png) }
        let data = try await loader().fetch(URL(string: "https://example.com/a.png")!)
        XCTAssertEqual(data, png)
        let request = try XCTUnwrap(ImageStubProtocol.seen.first)
        XCTAssertEqual(request.url?.scheme, "https")
        let headers = request.allHTTPHeaderFields ?? [:]
        for name in ["Authorization", "Cookie", "Referer", "X-RSI-CSRF", "X-CSRF-Token"] {
            XCTAssertNil(headers[name], name)
        }
        XCTAssertEqual(headers["Accept"], "image/*")
        XCTAssertFalse(request.httpShouldHandleCookies)
        XCTAssertNil(request.httpBody)
        XCTAssertEqual(request.httpMethod, "GET")
    }

    func testTheSessionHasNoStorageNoCacheAndNoDefaultHeaders() {
        let configuration = RemoteImageLoader().session.configuration
        XCTAssertNil(configuration.httpCookieStorage)
        XCTAssertFalse(configuration.httpShouldSetCookies)
        XCTAssertNil(configuration.urlCache)
        XCTAssertNil(configuration.urlCredentialStorage)
        XCTAssertEqual(configuration.httpAdditionalHeaders as? [String: String], ["Accept": "image/*"])
    }

    func testBlockedAddressesNeverReachTheNetwork() async {
        ImageStubProtocol.reset { _ in .init(body: self.png) }
        for raw in ["http://example.com/a.png", "https://192.168.1.2/a.png", "https://u:p@example.com/a.png", "https://hub.tail1.ts.net/a.png", "file:///a.png"] {
            do {
                _ = try await loader().fetch(URL(string: raw)!)
                XCTFail("fetched \(raw)")
            } catch let error as ImageLoadError {
                guard case .blocked = error else { return XCTFail("\(raw): \(error)") }
            } catch { XCTFail("\(error)") }
        }
        XCTAssertEqual(ImageStubProtocol.seen.count, 0)
    }

    func testNonImagesErrorsAndOversizeAreRefused() async {
        let big = String(ImagePolicy.maxRemoteBytes + 1)
        let cases: [(ImageStubProtocol.Reply, ImageLoadError)] = [
            (.init(headers: ["Content-Type": "text/html"], body: Data("<html>".utf8)), .notAnImage),
            (.init(headers: ["Content-Type": "image/svg+xml"], body: Data("<svg/>".utf8)), .notAnImage),
            (.init(status: 404, body: Data()), .http(404)),
            (.init(status: 500, body: Data()), .http(500)),
            (.init(headers: ["Content-Type": "image/png", "Content-Length": big], body: Data()), .tooLarge),
        ]
        for (reply, expected) in cases {
            ImageStubProtocol.reset { _ in reply }
            do {
                _ = try await loader().fetch(URL(string: "https://example.com/a.png")!)
                XCTFail("accepted \(expected)")
            } catch let error as ImageLoadError {
                XCTAssertEqual(error, expected)
            } catch { XCTFail("\(error)") }
        }
    }

    func testRedirectToAnAllowedSourceIsFollowedWithoutCredentials() async throws {
        ImageStubProtocol.reset { request in
            request.url?.host == "example.com"
                ? .init(status: 302, headers: ["Location": "https://cdn.example.net/a.png"])
                : .init(body: self.png)
        }
        let data = try await loader().fetch(URL(string: "https://example.com/a.png")!)
        XCTAssertEqual(data, png)
        XCTAssertEqual(ImageStubProtocol.seen.map { $0.url?.host }, ["example.com", "cdn.example.net"])
        for request in ImageStubProtocol.seen {
            XCTAssertNil(request.allHTTPHeaderFields?["Authorization"])
            XCTAssertNil(request.allHTTPHeaderFields?["Cookie"])
        }
    }

    func testRedirectsToPrivateOrInsecureSourcesAreRefused() async {
        for target in ["http://example.net/a.png", "https://192.168.0.9/a.png", "https://hub.tail1.ts.net/a.png", "file:///etc/passwd"] {
            ImageStubProtocol.reset { request in
                request.url?.host == "example.com" ? .init(status: 302, headers: ["Location": target]) : .init(body: self.png)
            }
            do {
                _ = try await loader().fetch(URL(string: "https://example.com/a.png")!)
                XCTFail("followed \(target)")
            } catch let error as ImageLoadError {
                XCTAssertEqual(error, .redirectRefused, target)
            } catch { XCTFail("\(target): \(error)") }
            XCTAssertEqual(ImageStubProtocol.seen.count, 1, target)
        }
    }

    func testLoadDecodesCachesAndReusesTheImage() async throws {
        ImageStubProtocol.reset { _ in .init(body: self.png) }
        let cache = ImageCache()
        let url = URL(string: "https://example.com/cached.png")!
        let first = try await loader().load(url, cache: cache)
        XCTAssertEqual(first.width, 96)
        _ = try await loader().load(url, cache: cache)
        XCTAssertEqual(ImageStubProtocol.seen.count, 1, "the second view is from memory")
    }

    func testAnUndecodableBodyIsDamagedNotACrash() async {
        ImageStubProtocol.reset { _ in .init(body: Data("not really a png".utf8)) }
        do {
            _ = try await loader().load(URL(string: "https://example.com/bad.png")!, cache: ImageCache())
            XCTFail("decoded garbage")
        } catch let error as ImageLoadError {
            XCTAssertEqual(error, .damaged)
        } catch { XCTFail("\(error)") }
    }
}

// MARK: - Zoom

final class ZoomStateTests: XCTestCase {
    func testPinchIsClampedAndResetsPanAtOne() {
        var zoom = ZoomState()
        zoom.pinch(from: 1, factor: 100)
        XCTAssertEqual(zoom.scale, ZoomState.maxScale)
        zoom.offset = CGSize(width: 50, height: 20)
        zoom.pinch(from: 1, factor: 0.1)
        XCTAssertEqual(zoom.scale, ZoomState.minScale)
        XCTAssertEqual(zoom.offset, .zero)
    }

    func testDoubleTapTogglesBetweenFitAndZoomed() {
        var zoom = ZoomState()
        zoom.doubleTap()
        XCTAssertEqual(zoom.scale, ZoomState.doubleTapScale)
        XCTAssertTrue(zoom.isZoomed)
        zoom.offset = CGSize(width: 10, height: 10)
        zoom.doubleTap()
        XCTAssertEqual(zoom, ZoomState())
    }

    func testPanCannotLeaveTheImage() {
        var zoom = ZoomState()
        let content = CGSize(width: 300, height: 200), viewport = CGSize(width: 300, height: 600)
        XCTAssertEqual(zoom.clamped(CGSize(width: 99, height: 99), content: content, viewport: viewport), .zero)
        zoom.scale = 2  // 600x400 in a 300x600 viewport: 150 of room sideways, none up or down
        XCTAssertEqual(zoom.clamped(CGSize(width: 999, height: 999), content: content, viewport: viewport), CGSize(width: 150, height: 0))
        XCTAssertEqual(zoom.clamped(CGSize(width: -999, height: 5), content: content, viewport: viewport), CGSize(width: -150, height: 0))
    }

    func testFittedSize() {
        XCTAssertEqual(fittedSize(aspect: 2, in: CGSize(width: 400, height: 800)), CGSize(width: 400, height: 200))
        XCTAssertEqual(fittedSize(aspect: 0.5, in: CGSize(width: 400, height: 400)), CGSize(width: 200, height: 400))
        XCTAssertEqual(fittedSize(aspect: 0, in: CGSize(width: 400, height: 400)), .zero)
    }
}

// MARK: - Rendering

/// Renders the fixture through the real view, to PNG files for the evidence note
/// ($RSI_SNAPSHOT_DIR, else /tmp/rsi-503-snapshots). `ImageRenderer` does not draw
/// scroll views, so code blocks and tables show as empty boxes here; the simulator
/// run shows them. The assertions are that views build and an image comes out.
@MainActor
final class MarkdownRenderTests: XCTestCase {
    /// `requireImage: false` for hostile input: the point there is that drawing it returns, not that
    /// the renderer can paint a 6-level-deep column of 6000 characters.
    private func render(_ name: String, text: String, theme: RemoteTheme, requireImage: Bool = true) throws {
        let view = MarkdownText(text: text)
            .padding(16)
            .frame(width: 390, alignment: .leading)
            .environment(\.theme, theme)
            .foregroundStyle(theme.primaryText.color)
            .tint(theme.primary.color)
            .background(theme.canvas.color)
            .environment(\.colorScheme, theme.isLight ? .light : .dark)
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        guard let png = renderer.uiImage?.pngData() else {
            if requireImage { XCTFail("no image for \(name)") }
            return
        }
        if requireImage { XCTAssertGreaterThan(png.count, 2_000) }
        let dir = ProcessInfo.processInfo.environment["RSI_SNAPSHOT_DIR"] ?? "/tmp/rsi-503-snapshots"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent(name + ".png"))
    }

    func testEveryFixtureMessageRendersInDarkAndLight() throws {
        let names = ["text", "lists", "table-quote", "code", "code2", "images", "degraded"]
        let messages = MarkdownFixture.parts
        XCTAssertEqual(names.count, messages.count)
        for theme in [RemoteTheme.goth, RemoteTheme.goth.lightVariant] {
            for (name, message) in zip(names, messages) {
                try render("md-\(theme.isLight ? "light" : "dark")-\(name)", text: message, theme: theme)
            }
        }
    }

    func testHostileAndHugeTextRendersWithoutTrapping() throws {
        let hostile = [
            String(repeating: "> ", count: 3_000) + "x", String(repeating: "- ", count: 3_000) + "x",
            "```\n" + String(repeating: "x", count: 100_000), String(repeating: "| a ", count: 500) + "|\n" + String(repeating: "|---", count: 500) + "|",
            "![x](" + String(repeating: "(", count: 5_000), "\u{0}\u{FFFF}\u{202E}rtl override",
        ]
        for (index, text) in hostile.enumerated() {
            try render("md-hostile-\(index)", text: text, theme: .goth, requireImage: false)
        }
    }
}
