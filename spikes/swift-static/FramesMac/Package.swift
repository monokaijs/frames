// swift-tools-version: 6.0
import PackageDescription

// Phase 0 spike 0-B: Swift platform services as a static library linked into
// the Rust binary. CFramesMac holds the C ABI header shared by Swift and Rust.
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
    ]
)
