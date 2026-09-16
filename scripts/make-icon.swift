// 畫 Kairos 的 app 圖示：Apple 圖示格線上的深色圓角方塊，中間一個白色 SF Symbol「stopwatch」，
// 跟選單列用的是同一個符號。輸出一整套 .iconset（16 到 1024 px），交給 iconutil 轉 icns。
//
// 用法：swift scripts/make-icon.swift <輸出 .iconset 目錄>（通常由 scripts/make-icon.sh 呼叫）

import AppKit

let args = CommandLine.arguments
guard args.count == 2 else {
    FileHandle.standardError.write("用法：swift make-icon.swift <輸出 .iconset 目錄>\n".data(using: .utf8)!)
    exit(2)
}
let outDir = URL(fileURLWithPath: args[1])
try? FileManager.default.removeItem(at: outDir)
try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)

/// 以 1024 為基準畫一份，縮到 `px × px` 的點陣圖；每個尺寸都重畫，不用縮圖。
func render(px: Int) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: px, height: px)
    NSGraphicsContext.saveGraphicsState()
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!
    NSGraphicsContext.current = ctx
    let s = CGFloat(px) / 1024.0
    ctx.cgContext.scaleBy(x: s, y: s)

    // Apple 的 macOS 圖示格線：1024 畫布，圓角方塊 824 × 824 置中，四邊各留 100。
    let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
    let path = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
    let top = NSColor(calibratedRed: 0.22, green: 0.24, blue: 0.30, alpha: 1)
    let bottom = NSColor(calibratedRed: 0.08, green: 0.09, blue: 0.12, alpha: 1)
    NSGradient(starting: top, ending: bottom)!.draw(in: path, angle: -90)

    // 內緣一圈淡淡的高光，讓它在深色桌面上有邊。
    NSColor(calibratedWhite: 1, alpha: 0.10).setStroke()
    let rim = NSBezierPath(roundedRect: tile.insetBy(dx: 6, dy: 6), xRadius: 179, yRadius: 179)
    rim.lineWidth = 12
    rim.stroke()

    let config = NSImage.SymbolConfiguration(pointSize: 520, weight: .regular)
        .applying(NSImage.SymbolConfiguration(paletteColors: [.white]))
    guard let base = NSImage(systemSymbolName: "stopwatch", accessibilityDescription: nil),
          let sym = base.withSymbolConfiguration(config)
    else {
        FileHandle.standardError.write("找不到 SF Symbol「stopwatch」\n".data(using: .utf8)!)
        exit(1)
    }
    let size = sym.size
    // 碼錶符號上面有按鈕，重心偏上，往下挪一點才看起來置中。
    let origin = NSPoint(x: tile.midX - size.width / 2, y: tile.midY - size.height / 2 - 8)
    sym.draw(in: NSRect(origin: origin, size: size))

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

// iconutil 要的檔名：icon_<pt>x<pt>[@2x].png。
let entries: [(name: String, px: Int)] = [
    ("icon_16x16", 16), ("icon_16x16@2x", 32),
    ("icon_32x32", 32), ("icon_32x32@2x", 64),
    ("icon_128x128", 128), ("icon_128x128@2x", 256),
    ("icon_256x256", 256), ("icon_256x256@2x", 512),
    ("icon_512x512", 512), ("icon_512x512@2x", 1024),
]
for e in entries {
    try render(px: e.px).write(to: outDir.appendingPathComponent("\(e.name).png"))
}
print("已輸出 \(entries.count) 張到 \(outDir.path)")
