// swift-tools-version: 6.0
import PackageDescription

// Frames v2 macOS platform services: ScreenCaptureKit capture, VideoToolbox
// encode/decode, and Metal presentation as a static library linked into the
// Rust binary (crates/frames-platform-macos). CFramesMac holds the C ABI.
let package = Package(
    name: "FramesMac",
    platforms: [.macOS(.v14)],
    products: [.library(name: "FramesMac", type: .static, targets: ["FramesMac"])],
    targets: [
        .target(name: "CFramesMac"),
        .target(
            name: "FramesMac",
            dependencies: ["CFramesMac"],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
        .testTarget(
            name: "FramesMacTests",
            dependencies: ["FramesMac", "CFramesMac"],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
    ]
)
