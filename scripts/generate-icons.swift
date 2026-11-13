// Run on macOS: swift scripts/generate-icons.swift
// No image tools or third-party dependencies required. Coordinates are 0...1024.
import AppKit

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
let assets = root.appendingPathComponent("assets")
let iconset = assets.appendingPathComponent("Gander.iconset")
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func drawIcon() {
    let tile = NSBezierPath(roundedRect: NSRect(x: 64, y: 64, width: 896, height: 896), xRadius: 200, yRadius: 200)
    NSGradient(starting: NSColor(srgbRed: 0.12, green: 0.31, blue: 0.34, alpha: 1),
               ending: NSColor(srgbRed: 0.05, green: 0.15, blue: 0.20, alpha: 1))!
        .draw(in: tile, angle: -90)

    // A watchful goose: broad body, long neck, and a small amber bill.
    let goose = NSBezierPath()
    goose.move(to: NSPoint(x: 211, y: 360))
    goose.curve(to: NSPoint(x: 563, y: 266), controlPoint1: NSPoint(x: 265, y: 235), controlPoint2: NSPoint(x: 453, y: 211))
    goose.curve(to: NSPoint(x: 682, y: 465), controlPoint1: NSPoint(x: 663, y: 311), controlPoint2: NSPoint(x: 682, y: 381))
    goose.line(to: NSPoint(x: 682, y: 654))
    goose.curve(to: NSPoint(x: 744, y: 718), controlPoint1: NSPoint(x: 682, y: 689), controlPoint2: NSPoint(x: 710, y: 718))
    goose.curve(to: NSPoint(x: 651, y: 801), controlPoint1: NSPoint(x: 746, y: 775), controlPoint2: NSPoint(x: 705, y: 810))
    goose.curve(to: NSPoint(x: 557, y: 683), controlPoint1: NSPoint(x: 592, y: 798), controlPoint2: NSPoint(x: 557, y: 747))
    goose.line(to: NSPoint(x: 557, y: 490))
    goose.curve(to: NSPoint(x: 481, y: 420), controlPoint1: NSPoint(x: 557, y: 443), controlPoint2: NSPoint(x: 527, y: 420))
    goose.curve(to: NSPoint(x: 211, y: 360), controlPoint1: NSPoint(x: 377, y: 462), controlPoint2: NSPoint(x: 296, y: 385))
    goose.close()
    NSColor(srgbRed: 0.96, green: 0.95, blue: 0.88, alpha: 1).setFill()
    goose.fill()

    let wing = NSBezierPath()
    wing.move(to: NSPoint(x: 299, y: 353))
    wing.curve(to: NSPoint(x: 528, y: 348), controlPoint1: NSPoint(x: 385, y: 315), controlPoint2: NSPoint(x: 471, y: 308))
    wing.lineWidth = 22
    wing.lineCapStyle = .round
    NSColor(srgbRed: 0.69, green: 0.77, blue: 0.73, alpha: 1).setStroke()
    wing.stroke()

    let bill = NSBezierPath()
    bill.move(to: NSPoint(x: 726, y: 740))
    bill.line(to: NSPoint(x: 817, y: 711))
    bill.curve(to: NSPoint(x: 735, y: 684), controlPoint1: NSPoint(x: 828, y: 701), controlPoint2: NSPoint(x: 770, y: 685))
    bill.close()
    NSColor(srgbRed: 0.96, green: 0.64, blue: 0.27, alpha: 1).setFill()
    bill.fill()
    NSColor(srgbRed: 0.06, green: 0.17, blue: 0.20, alpha: 1).setFill()
    NSBezierPath(ovalIn: NSRect(x: 687, y: 742, width: 18, height: 18)).fill()
}

func png(size: Int) -> Data {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
                                  bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                  isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    let transform = AffineTransform(scale: CGFloat(size) / 1024)
    (transform as NSAffineTransform).concat()
    drawIcon()
    NSGraphicsContext.restoreGraphicsState()
    return bitmap.representation(using: .png, properties: [:])!
}

for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let suffix = scale == 2 ? "@2x" : ""
        try png(size: size * scale).write(to: iconset.appendingPathComponent("icon_\(size)x\(size)\(suffix).png"))
    }
}
try png(size: 1024).write(to: assets.appendingPathComponent("Gander.png"))
let process = Process()
process.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
process.arguments = ["-c", "icns", iconset.path, "-o", assets.appendingPathComponent("Gander.icns").path]
try process.run()
process.waitUntilExit()
guard process.terminationStatus == 0 else { fatalError("iconutil failed") }
print("Generated assets/Gander.png, Gander.iconset, and Gander.icns")
