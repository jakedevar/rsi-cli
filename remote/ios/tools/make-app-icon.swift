// Renders the RSI Remote app icon: a rainbow golden-ratio spiral on black.
//   swift remote/ios/tools/make-app-icon.swift <out.png> [size]
// The spiral is logarithmic, r = a * phi^(2*theta/pi): it grows by the golden
// ratio every quarter turn. Hue runs the rainbow along its arc length; the
// stroke widens with the radius and carries a soft glow of its own colour.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write("usage: make-app-icon.swift <out.png> [size]\n".data(using: .utf8)!)
    exit(2)
}
let size = args.count > 2 ? Int(args[2]) ?? 1024 : 1024
let s = CGFloat(size)
let phi = (1 + sqrt(5.0)) / 2

// Spiral points, inside out: three and a half visible turns. Inner turns smaller
// than a few pixels add nothing, so the spiral starts at r = 1 (theta = 0).
let turns = 3.5
let thetaMax = 2 * Double.pi * turns
let steps = 3000
func radius(_ theta: Double) -> Double { pow(phi, 2 * theta / .pi) }
var raw: [(x: Double, y: Double, r: Double)] = []
for i in 0...steps {
    let theta = Double(i) / Double(steps) * thetaMax
    let r = radius(theta)
    let a = theta + 1.35 * .pi
    raw.append((r * cos(a), r * sin(a), r))
}
// Fit the spiral's bounding box (with room for the widest stroke) into the icon, centred.
let xs = raw.map(\.x), ys = raw.map(\.y)
let (minX, maxX, minY, maxY) = (xs.min()!, xs.max()!, ys.min()!, ys.max()!)
let margin = 0.17 * Double(s)
let scale = min((Double(s) - 2 * margin) / (maxX - minX), (Double(s) - 2 * margin) / (maxY - minY))
let offX = (Double(s) - (maxX - minX) * scale) / 2 - minX * scale
let offY = (Double(s) - (maxY - minY) * scale) / 2 - minY * scale
// Arc length along the spiral, so the rainbow is spread over what the eye sees.
var pts: [(p: CGPoint, r: Double, u: Double)] = []
var length = 0.0
for (i, q) in raw.enumerated() {
    let p = CGPoint(x: q.x * scale + offX, y: q.y * scale + offY)
    if i > 0 { length += hypot(p.x - pts[i - 1].p.x, p.y - pts[i - 1].p.y) }
    pts.append((p, q.r * scale, length))
}
pts = pts.map { ($0.p, $0.r, $0.u / length) }

func rgb(hue h: Double, sat: Double = 1, val: Double = 1) -> CGColor {
    let h = h - floor(h)
    let i = Int(h * 6) % 6, f = h * 6 - floor(h * 6)
    let p = val * (1 - sat), q = val * (1 - f * sat), u = val * (1 - (1 - f) * sat)
    let (r, g, b): (Double, Double, Double) = [(val, u, p), (q, val, p), (p, val, u), (p, q, val), (u, p, val), (val, p, q)][i]
    return CGColor(srgbRed: r, green: g, blue: b, alpha: 1)
}

let space = CGColorSpace(name: CGColorSpace.sRGB)!
guard let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                          space: space, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { exit(1) }
ctx.setFillColor(CGColor(srgbRed: 0, green: 0, blue: 0, alpha: 1))
ctx.fill(CGRect(x: 0, y: 0, width: s, height: s))
ctx.setLineCap(.round)
ctx.setLineJoin(.round)

// Width in proportion to the radius, like a nautilus: successive turns stay apart.
func width(_ r: Double) -> CGFloat { CGFloat(max(0.004 * Double(s), 0.17 * r)) }
// Violet at the eye through blue, green, yellow and orange to red at the outer arm.
func hue(_ u: Double) -> Double { 0.80 * (1 - u) }

func pass(_ draw: (CGPoint, CGPoint, Double, Double) -> Void) {
    for i in 1..<pts.count { draw(pts[i - 1].p, pts[i].p, pts[i].r, pts[i].u) }
}
// 1. A soft glow of the stroke's own colour.
pass { p0, p1, r, u in
    ctx.saveGState()
    let c = rgb(hue: hue(u), sat: 1, val: 1)
    ctx.setShadow(offset: .zero, blur: 0.03 * s, color: c.copy(alpha: 0.75))
    ctx.setStrokeColor(c.copy(alpha: 0.25)!)
    ctx.setLineWidth(width(r))
    ctx.move(to: p0); ctx.addLine(to: p1); ctx.strokePath()
    ctx.restoreGState()
}
// 2. The stroke.
pass { p0, p1, r, u in
    ctx.setStrokeColor(rgb(hue: hue(u), sat: 0.9, val: 1))
    ctx.setLineWidth(width(r))
    ctx.move(to: p0); ctx.addLine(to: p1); ctx.strokePath()
}
// 3. A faint highlight along the inner edge, for depth.
pass { p0, p1, r, u in
    ctx.setStrokeColor(rgb(hue: hue(u), sat: 0.45, val: 1).copy(alpha: 0.25)!)
    ctx.setLineWidth(max(1, width(r) * 0.28))
    ctx.move(to: p0); ctx.addLine(to: p1); ctx.strokePath()
}

guard let image = ctx.makeImage(),
      let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, UTType.png.identifier as CFString, 1, nil)
else { exit(1) }
CGImageDestinationAddImage(dest, image, nil)
exit(CGImageDestinationFinalize(dest) ? 0 : 1)
