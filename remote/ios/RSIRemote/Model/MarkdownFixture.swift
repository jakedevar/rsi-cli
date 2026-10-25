import CoreGraphics
import Foundation
import ImageIO

// A conversation that holds every element the message renderer draws (#503), as
// separate messages (the hub bounds one event at 4096 bytes). The tests render it
// and the launch argument `-markdownDemo 1` shows it on a phone or simulator
// without any hub; nothing here is fetched, and the one remote image is only a
// placeholder until tapped.

enum MarkdownFixture {
    static let question =
        "Show me everything the message view can draw: headings, lists, a table, quotes, code, links and images."

    static let text = """
        # Release notes
        ## What changed
        ### Messages
        #### Details

        This is **bold**, *italic*, ~~struck~~, `inline code`, a [link](https://example.com/docs) and a bare address https://example.com/status.
        Line breaks
        are kept.
        """

    static let lists = """
        Lists, nested lists and tasks:

        - First bullet
        - Second bullet with **bold**
          - Nested bullet
            - Third level
        - [x] Done task
        - [ ] Open task

        1. First step
        2. Second step
           1. Nested numbered
        3. Third step
        """

    static let tableQuoteRule = """
        | Check | Result | Time |
        | :--- | :---: | ---: |
        | lint | pass | 1.2s |
        | unit tests | pass | 48.0s |
        | `scoped_test` | **fail** | 3m 10s |

        > A quote from the review:
        > > and a nested quote
        > back in the first.

        ---

        After the rule: a paragraph.
        """

    static let swiftCode = """
        ```swift
        // Parse a wire time.
        struct Receipt: Codable {
            let id: String
            var attempts = 3
        }
        func load(_ url: URL) async throws -> Receipt {
            let (data, _) = try await URLSession.shared.data(from: url)
            return try JSONDecoder().decode(Receipt.self, from: data)
        }
        ```

        ```python
        import json
        def greet(name: str) -> str:
            \"\"\"Say hello.\"\"\"
            return f"hello {name}"  # a comment
        print(json.dumps({"ok": True, "n": 42}))
        ```
        """

    static let otherCode = """
        ```json
        {"status": "ok", "count": 3, "items": [true, null, 1.5]}
        ```

        ```bash
        # build it
        for f in *.swift; do echo "$f" && wc -l "$f"; done
        ```

        ```diff
        @@ -1,3 +1,3 @@
         context
        -old line
        +new line
        ```

        ```
        plain output with no language
        ```
        """

    static let images = """
        An inline image (a data URI, drawn without any network):

        ![Colour test card](\(sampleImageDataURI))

        A public image waits for a tap, and says where it would come from:

        ![A diagram on a public site](https://example.com/diagram.png)

        https://example.com/photos/cover.jpg

        These are refused, each with its reason:

        ![local](http://192.168.1.5/camera.png)
        ![hub](https://my-mac.tail1234.ts.net/secret.png)
        ![odd](ftp://example.com/a.png)
        """

    static let degraded = """
        Things that are not valid stay readable:

        ```swift
        let unclosed = "fence"

        | not | a table
        | broken

        ![broken image](
        <div style="x">raw html stays text</div> and **unclosed bold
        """

    /// The assistant's messages, in order; each is one screen or less.
    static var parts: [String] { [text, lists, tableQuoteRule, swiftCode, otherCode, images, degraded] }

    /// The conversation: the operator asks, the assistant answers in parts.
    static var events: [ChatEvent] {
        let answers = parts
        var events = [
            ChatEvent(
                id: "fx-0", sequence: 1, role: .user, text: question, createdAt: "2026-10-22T18:00:00Z")
        ]
        for (index, answer) in answers.enumerated() {
            events.append(
                ChatEvent(
                    id: "fx-\(index + 1)", sequence: index + 2, role: .assistant, text: answer,
                    createdAt: "2026-10-22T18:00:0\(index + 1)Z"))
        }
        return events
    }

    /// A small PNG test card as a `data:` URI, drawn with CoreGraphics so the same
    /// bytes come out on the phone, the simulator and the macOS harness.
    static var sampleImageDataURI: String {
        "data:image/png;base64," + samplePNG().base64EncodedString()
    }

    static func samplePNG(width: Int = 96, height: Int = 54) -> Data {
        let space = CGColorSpaceCreateDeviceRGB()
        guard
            let context = CGContext(
                data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return Data() }
        let bars: [(CGFloat, CGFloat, CGFloat)] = [
            (0.95, 0.30, 0.30), (0.95, 0.65, 0.20), (0.95, 0.90, 0.30), (0.35, 0.80, 0.40),
            (0.25, 0.65, 0.95), (0.55, 0.40, 0.95), (0.9, 0.9, 0.9),
        ]
        let barWidth = CGFloat(width) / CGFloat(bars.count)
        for (index, color) in bars.enumerated() {
            context.setFillColor(red: color.0, green: color.1, blue: color.2, alpha: 1)
            context.fill(CGRect(x: CGFloat(index) * barWidth, y: 0, width: barWidth + 1, height: CGFloat(height)))
        }
        context.setFillColor(red: 0.1, green: 0.1, blue: 0.15, alpha: 1)
        context.fill(CGRect(x: 0, y: 0, width: width, height: height / 5))
        guard let image = context.makeImage() else { return Data() }
        let data = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(data, "public.png" as CFString, 1, nil) else {
            return Data()
        }
        CGImageDestinationAddImage(destination, image, nil)
        CGImageDestinationFinalize(destination)
        return data as Data
    }
}
