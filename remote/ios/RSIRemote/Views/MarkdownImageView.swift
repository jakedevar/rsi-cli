import SwiftUI

/// An image a message names (#503). A `data:` image decodes when its row is
/// drawn; a public https image shows a placeholder naming the host and loads only
/// when the reader taps it (the policy and the reason are in
/// Model/MarkdownImages.swift); anything else says why it is not shown. A loaded
/// image opens full screen with pinch and double-tap zoom.
struct MarkdownImageView: View {
    @Environment(\.theme) private var theme
    let alt: String
    let source: String
    @Environment(\.markdownDemoOpensZoom) private var opensZoom
    var loader: RemoteImageLoader = .shared

    enum Phase {
        case checking
        case idle(URL)
        case loading(URL)
        case loaded(DecodedImage)
        case failed(URL?, String)
        case blocked(String)
    }

    @State private var phase: Phase = .checking
    @State private var zoomed: ZoomItem?

    struct ZoomItem: Identifiable {
        let id = UUID()
        let image: DecodedImage
    }

    var body: some View {
        content
            .task(id: source) { await resolve() }
            .fullScreenCover(item: $zoomed) { item in
                ImageZoomView(image: item.image, label: label)
            }
    }

    private var label: String { alt.isEmpty ? "Image" : alt }

    @ViewBuilder private var content: some View {
        switch phase {
        case .checking:
            placeholder { ProgressView() }
        case .idle(let url):
            Button {
                Task { await fetch(url) }
            } label: {
                placeholder {
                    VStack(spacing: 4) {
                        Image(systemName: "photo").font(.title3)
                        Text("Tap to load image").font(.footnote.weight(.semibold))
                        Text(url.host() ?? "").font(.caption2).foregroundStyle(theme.dimText.color)
                        if !alt.isEmpty {
                            Text(alt).font(.caption2).lineLimit(2).foregroundStyle(theme.mutedText.color)
                        }
                    }
                    .padding(8)
                }
            }
            .buttonStyle(.plain)
            .accessibilityLabel("\(label), image from \(url.host() ?? "the web"). Double tap to load.")
        case .loading:
            placeholder { ProgressView() }
        case .loaded(let image):
            Image(decorative: image.cgImage, scale: 1)
                .resizable().scaledToFit()
                .frame(maxWidth: .infinity, maxHeight: 360, alignment: .leading)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                .contentShape(Rectangle())
                .onTapGesture { zoomed = ZoomItem(image: image) }
                .accessibilityLabel(label)
                .accessibilityHint("Double tap to zoom")
                .accessibilityAddTraits(.isImage)
        case .failed(let url, let message):
            note(icon: "exclamationmark.triangle", text: message) {
                if let url {
                    Button("Try again") { Task { await fetch(url) } }
                        .font(.caption.weight(.semibold))
                }
            }
        case .blocked(let reason):
            note(icon: "photo.badge.exclamationmark", text: "Image not shown: \(reason)") {
                if !alt.isEmpty { Text(alt).font(.caption2).foregroundStyle(theme.dimText.color) }
            }
        }
    }

    private func placeholder<Inner: View>(@ViewBuilder _ inner: () -> Inner) -> some View {
        inner()
            .frame(maxWidth: .infinity, minHeight: 120)
            .background(theme.panel.color, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(theme.border.color.opacity(0.4), style: StrokeStyle(lineWidth: 1, dash: [4, 3])))
            .foregroundStyle(theme.mutedText.color)
    }

    private func note<Extra: View>(icon: String, text: String, @ViewBuilder _ extra: () -> Extra) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Label(text, systemImage: icon).font(.caption).foregroundStyle(theme.mutedText.color)
            extra()
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(theme.panel.color, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    /// Classify the source; decode an inline image, look a remote one up in the
    /// cache, otherwise wait for the tap.
    private func resolve() async {
        switch ImagePolicy.classify(source) {
        case .blocked(let reason):
            phase = .blocked(reason)
        case .inline(let data):
            if let hit = ImageCache.shared.image(for: source) {
                phase = .loaded(hit)
                return
            }
            phase = .checking
            let decoded = await Task.detached(priority: .userInitiated) { ImageDecoding.decode(data) }.value
            if Task.isCancelled { return }
            if let decoded {
                ImageCache.shared.store(decoded, for: source)
                phase = .loaded(decoded)
                if opensZoom { zoomed = ZoomItem(image: decoded) }
            } else {
                phase = .failed(nil, "The image could not be read")
            }
        case .remote(let url):
            if let hit = ImageCache.shared.image(for: url.absoluteString) {
                phase = .loaded(hit)
            } else {
                phase = .idle(url)
            }
        }
    }

    private func fetch(_ url: URL) async {
        phase = .loading(url)
        do {
            phase = .loaded(try await loader.load(url))
        } catch is CancellationError {
            phase = .idle(url)
        } catch let error as ImageLoadError {
            phase = .failed(url, error.message)
        } catch {
            phase = .failed(url, "Could not load")
        }
    }
}

/// The largest size with `aspect` that fits in `viewport`.
func fittedSize(aspect: CGFloat, in viewport: CGSize) -> CGSize {
    guard aspect > 0, viewport.width > 0, viewport.height > 0 else { return .zero }
    let byWidth = CGSize(width: viewport.width, height: viewport.width / aspect)
    if byWidth.height <= viewport.height { return byWidth }
    return CGSize(width: viewport.height * aspect, height: viewport.height)
}

/// The full-screen image: pinch to zoom, double tap to toggle, drag to pan when
/// zoomed, a close button.
struct ImageZoomView: View {
    @Environment(\.dismiss) private var dismiss
    let image: DecodedImage
    let label: String

    @State private var zoom = ZoomState()
    @State private var pinchBase: CGFloat = 1
    @State private var panBase: CGSize = .zero

    var body: some View {
        GeometryReader { geometry in
            let fit = fittedSize(aspect: image.aspect, in: geometry.size)
            ZStack {
                Color.black.ignoresSafeArea()
                Image(decorative: image.cgImage, scale: 1)
                    .resizable().scaledToFit()
                    .frame(width: fit.width, height: fit.height)
                    .scaleEffect(zoom.scale)
                    .offset(zoom.offset)
                    .gesture(
                        MagnifyGesture()
                            .onChanged { value in
                                zoom.pinch(from: pinchBase, factor: value.magnification)
                                zoom.offset = zoom.clamped(zoom.offset, content: fit, viewport: geometry.size)
                            }
                            .onEnded { _ in
                                pinchBase = zoom.scale
                                panBase = zoom.offset
                            }
                    )
                    .simultaneousGesture(
                        DragGesture()
                            .onChanged { value in
                                guard zoom.isZoomed else { return }
                                let proposed = CGSize(
                                    width: panBase.width + value.translation.width,
                                    height: panBase.height + value.translation.height)
                                zoom.offset = zoom.clamped(proposed, content: fit, viewport: geometry.size)
                            }
                            .onEnded { _ in panBase = zoom.offset }
                    )
                    .onTapGesture(count: 2) {
                        withAnimation(.snappy(duration: 0.25)) {
                            zoom.doubleTap()
                            pinchBase = zoom.scale
                            panBase = zoom.offset
                        }
                    }
                    .accessibilityLabel(label)
            }
            .overlay(alignment: .topTrailing) {
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 30)).symbolRenderingMode(.palette)
                        .foregroundStyle(.white, .white.opacity(0.3))
                        .frame(width: 44, height: 44)
                }
                .padding(8)
                .accessibilityLabel("Close")
            }
        }
        .statusBarHidden()
    }
}
