import CoreGraphics
import Foundation
import ImageIO

// Images in messages (#503). The Remote read projection carries no image or
// attachment fields (events are bounded text), so the only images the app can see
// are the ones the text itself names: Markdown `![alt](url)` and a bare image URL
// on its own line. Policy, in one place:
//
// - `data:image/...;base64,` images are self-contained: no network, decoded when
//   the row is drawn.
// - `https://` images on a public DNS name are NEVER fetched automatically. A
//   message (a model's output, possibly steered by hostile text it read) must not
//   make the phone call a chosen server: that leaks the phone's address and
//   "is anyone reading this" to whoever wrote the URL. The reader taps the
//   placeholder, which names the host, and only then is one request made.
// - That request carries nothing of ours: its own ephemeral session, no cookies,
//   no credentials, no cache, no `Authorization`, never the hub token (which only
//   `RemoteClient`'s session holds), no referrer. Redirects are followed only to
//   sources that pass the same policy.
// - Everything else is refused with a reason shown in the row: `http`, `file`,
//   other schemes, URLs with a user name, ports other than 443, IP addresses,
//   `localhost`, single-label and private-suffix names (`.local`, `.internal`,
//   `.lan`, `.ts.net` -- the operator's own network), svg.

enum ImageSource: Equatable, Sendable {
    /// A `data:` image, decoded locally.
    case inline(Data)
    /// A public https image, loaded when the reader taps.
    case remote(URL)
    /// Refused; the string is the reason shown to the reader.
    case blocked(String)
}

enum ImagePolicy {
    static let maxInlineBytes = 4 * 1024 * 1024
    static let maxRemoteBytes = 6 * 1024 * 1024
    static let maxRemoteURLLength = 2048
    static let maxInlineURILength = 100_000
    static let maxPixels = 64_000_000
    static let maxRedirects = 3
    private static let inlineTypes: Set<String> = ["png", "jpeg", "jpg", "gif", "webp", "heic"]
    private static let privateSuffixes = [
        "local", "internal", "lan", "home", "localdomain", "localhost", "ts.net", "corp",
        "intranet", "private", "home.arpa", "onion", "invalid", "test",
    ]

    static func classify(_ raw: String) -> ImageSource {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return .blocked("no address") }
        if text.lowercased().hasPrefix("data:") { return classifyInline(text) }
        guard text.count <= maxRemoteURLLength else { return .blocked("address too long") }
        guard let url = URL(string: text), let scheme = url.scheme?.lowercased() else {
            return .blocked("not a web address")
        }
        guard scheme == "https" else {
            return .blocked(scheme == "http" ? "not https" : "unsupported address (\(scheme))")
        }
        guard url.user() == nil, url.password() == nil else { return .blocked("address has a user name") }
        guard url.port == nil || url.port == 443 else { return .blocked("unusual port") }
        guard let host = url.host()?.lowercased(), isPublicName(host) else {
            return .blocked("not a public site")
        }
        if url.pathExtension.lowercased() == "svg" { return .blocked("svg images are not shown") }
        return .remote(url)
    }

    private static func classifyInline(_ text: String) -> ImageSource {
        guard text.count <= maxInlineURILength else { return .blocked("inline image too large") }
        let lower = text.lowercased()
        guard lower.hasPrefix("data:image/"), let comma = text.firstIndex(of: ",") else {
            return .blocked("unsupported inline data")
        }
        let header = lower[lower.index(lower.startIndex, offsetBy: 11)..<comma]
        let parts = header.split(separator: ";", omittingEmptySubsequences: false)
        guard let type = parts.first, inlineTypes.contains(String(type)), parts.last == "base64" else {
            return .blocked("unsupported inline image")
        }
        let payload = String(text[text.index(after: comma)...])
        guard let data = Data(base64Encoded: payload, options: .ignoreUnknownCharacters), !data.isEmpty
        else { return .blocked("inline image is damaged") }
        guard data.count <= maxInlineBytes else { return .blocked("inline image too large") }
        return .inline(data)
    }

    /// A DNS name on the public internet: at least two labels, an alphabetic top
    /// label, not an address and not one of the private suffixes.
    static func isPublicName(_ rawHost: String) -> Bool {
        var host = rawHost.lowercased()
        if host.hasSuffix(".") { host.removeLast() }
        guard !host.isEmpty, host.count <= 253, !host.contains(":"), !host.contains("[") else { return false }
        let labels = host.split(separator: ".", omittingEmptySubsequences: false)
        guard labels.count >= 2, labels.allSatisfy({ !$0.isEmpty && $0.count <= 63 }) else { return false }
        for label in labels {
            guard label.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") }),
                !label.hasPrefix("-"), !label.hasSuffix("-")
            else { return false }
        }
        let top = String(labels[labels.count - 1])
        guard top.count >= 2, top.allSatisfy({ $0.isLetter }) || top.hasPrefix("xn--") else { return false }
        for suffix in privateSuffixes where host == suffix || host.hasSuffix("." + suffix) { return false }
        return true
    }
}

// MARK: - Decoding

/// A decoded, size-bounded image.
struct DecodedImage: @unchecked Sendable {
    let cgImage: CGImage
    var width: Int { cgImage.width }
    var height: Int { cgImage.height }
    var aspect: CGFloat { height == 0 ? 1 : CGFloat(width) / CGFloat(height) }
}

enum ImageDecoding {
    /// Longest side kept after decoding; a photo is never decoded at full size.
    static let maxPixelSize = 2048

    /// Decodes with ImageIO, first frame only, downsampled, refusing absurd
    /// dimensions before allocating. Nil for anything that is not a bitmap image.
    static func decode(_ data: Data) -> DecodedImage? {
        let options = [kCGImageSourceShouldCache: false] as CFDictionary
        guard let source = CGImageSourceCreateWithData(data as CFData, options),
            CGImageSourceGetCount(source) > 0,
            let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
            let width = properties[kCGImagePropertyPixelWidth] as? Int,
            let height = properties[kCGImagePropertyPixelHeight] as? Int,
            width > 0, height > 0, width * height <= ImagePolicy.maxPixels
        else { return nil }
        let thumbnail =
            [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceShouldCacheImmediately: true,
                kCGImageSourceThumbnailMaxPixelSize: maxPixelSize,
            ] as CFDictionary
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, thumbnail) else { return nil }
        return DecodedImage(cgImage: image)
    }
}

/// Decoded images by source, bounded; memory only (nothing is written to disk).
final class ImageCache: @unchecked Sendable {
    static let shared = ImageCache()
    private final class Box {
        let image: DecodedImage
        init(_ image: DecodedImage) { self.image = image }
    }
    private let cache = NSCache<NSString, Box>()

    init(countLimit: Int = 48, byteLimit: Int = 96 * 1024 * 1024) {
        cache.countLimit = countLimit
        cache.totalCostLimit = byteLimit
    }

    func image(for key: String) -> DecodedImage? { cache.object(forKey: key as NSString)?.image }

    func store(_ image: DecodedImage, for key: String) {
        cache.setObject(Box(image), forKey: key as NSString, cost: image.width * image.height * 4)
    }
}

// MARK: - Loading a remote image

enum ImageLoadError: Error, Equatable, Sendable {
    case blocked(String)
    case http(Int)
    case redirectRefused
    case notAnImage
    case tooLarge
    case damaged
    case network(String)

    var message: String {
        switch self {
        case .blocked(let why): "Not loaded: \(why)"
        case .http(let code): "The site answered \(code)"
        case .redirectRefused: "The site redirected somewhere not allowed"
        case .notAnImage: "The address is not an image"
        case .tooLarge: "The image is too large"
        case .damaged: "The image could not be read"
        case .network(let text): text
        }
    }
}

/// Fetches one public image on a session that carries nothing of ours.
final class RemoteImageLoader: Sendable {
    static let shared = RemoteImageLoader()
    let session: URLSession

    /// `protocolClasses` is for tests only.
    init(protocolClasses: [AnyClass]? = nil) {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.httpCookieAcceptPolicy = .never
        configuration.urlCache = nil
        configuration.urlCredentialStorage = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
        configuration.httpAdditionalHeaders = ["Accept": "image/*"]
        configuration.timeoutIntervalForRequest = 15
        configuration.timeoutIntervalForResource = 40
        configuration.waitsForConnectivity = false
        configuration.tlsMinimumSupportedProtocolVersion = .TLSv12
        if let protocolClasses { configuration.protocolClasses = protocolClasses }
        session = URLSession(configuration: configuration)
    }

    /// The request this loader sends for `url`: nothing but the address and an
    /// `Accept: image/*`.
    static func request(for url: URL) -> URLRequest {
        var request = URLRequest(
            url: url, cachePolicy: .reloadIgnoringLocalAndRemoteCacheData, timeoutInterval: 15)
        request.httpShouldHandleCookies = false
        request.setValue("image/*", forHTTPHeaderField: "Accept")
        request.setValue("RSI-Remote", forHTTPHeaderField: "User-Agent")
        return request
    }

    func fetch(_ url: URL) async throws -> Data {
        guard case .remote(let allowed) = ImagePolicy.classify(url.absoluteString) else {
            throw ImageLoadError.blocked("not allowed")
        }
        let guardRedirects = RedirectGuard()
        do {
            let (bytes, response) = try await session.bytes(
                for: Self.request(for: allowed), delegate: guardRedirects)
            guard let http = response as? HTTPURLResponse else { throw ImageLoadError.notAnImage }
            if (300..<400).contains(http.statusCode) { throw ImageLoadError.redirectRefused }
            guard (200..<300).contains(http.statusCode) else { throw ImageLoadError.http(http.statusCode) }
            let type = (http.mimeType ?? "").lowercased()
            guard type.hasPrefix("image/"), type != "image/svg+xml" else { throw ImageLoadError.notAnImage }
            if http.expectedContentLength > Int64(ImagePolicy.maxRemoteBytes) { throw ImageLoadError.tooLarge }
            var data = Data()
            for try await byte in bytes {
                data.append(byte)
                if data.count > ImagePolicy.maxRemoteBytes { throw ImageLoadError.tooLarge }
            }
            return data
        } catch let error as ImageLoadError {
            throw error
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            if guardRedirects.refused { throw ImageLoadError.redirectRefused }
            let code = (error as? URLError)?.code
            throw ImageLoadError.network(
                code == .notConnectedToInternet ? "No connection" : code == .timedOut ? "Timed out" : "Could not load")
        }
    }

    /// Fetch, then decode (downsampled); the decoded image is cached under the URL.
    func load(_ url: URL, cache: ImageCache = .shared) async throws -> DecodedImage {
        if let hit = cache.image(for: url.absoluteString) { return hit }
        let data = try await fetch(url)
        guard let image = ImageDecoding.decode(data) else { throw ImageLoadError.damaged }
        cache.store(image, for: url.absoluteString)
        return image
    }
}

/// Follows a redirect only to a source the policy allows, a few times, and never
/// forwards a credential or cookie header.
final class RedirectGuard: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var hops = 0
    private var wasRefused = false

    var refused: Bool {
        lock.lock()
        defer { lock.unlock() }
        return wasRefused
    }

    func urlSession(
        _ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void
    ) {
        lock.lock()
        hops += 1
        let tooMany = hops > ImagePolicy.maxRedirects
        lock.unlock()
        guard !tooMany, let url = request.url, case .remote = ImagePolicy.classify(url.absoluteString) else {
            lock.lock()
            wasRefused = true
            lock.unlock()
            completionHandler(nil)
            task.cancel()  // do not wait for a body from a site we refuse to follow
            return
        }
        var next = request
        next.setValue(nil, forHTTPHeaderField: "Authorization")
        next.setValue(nil, forHTTPHeaderField: "Cookie")
        completionHandler(next)
    }
}

// MARK: - Zoom

/// Pinch and double-tap zoom of the full-screen image, as plain arithmetic.
struct ZoomState: Equatable, Sendable {
    static let minScale: CGFloat = 1
    static let maxScale: CGFloat = 6
    static let doubleTapScale: CGFloat = 2.5

    var scale: CGFloat = 1
    var offset: CGSize = .zero

    var isZoomed: Bool { scale > 1.01 }

    /// A pinch that started at `base` and has scaled by `factor` so far.
    mutating func pinch(from base: CGFloat, factor: CGFloat) {
        scale = min(Self.maxScale, max(Self.minScale, base * factor))
        if !isZoomed { offset = .zero }
    }

    mutating func doubleTap() {
        if isZoomed {
            self = ZoomState()
        } else {
            scale = Self.doubleTapScale
        }
    }

    /// `proposed` pan limited so the image edge never moves inside the viewport.
    func clamped(_ proposed: CGSize, content: CGSize, viewport: CGSize) -> CGSize {
        let overX = max(0, (content.width * scale - viewport.width) / 2)
        let overY = max(0, (content.height * scale - viewport.height) / 2)
        return CGSize(
            width: min(overX, max(-overX, proposed.width)),
            height: min(overY, max(-overY, proposed.height)))
    }
}
