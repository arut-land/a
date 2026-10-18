// swift-tools-version:6.2
import PackageDescription

// The thin layer over the generated bindings (../Generated/AFfi, from
// `mise run apple:ffi`): Sendable for the Rust objects, which lock inside,
// and the app's stores, isolated to the main actor by default.
let package = Package(
    name: "AKit",
    platforms: [.macOS(.v15)],
    products: [.library(name: "AKit", targets: ["AKit"])],
    dependencies: [.package(path: "../Generated/AFfi")],
    targets: [
        .target(
            name: "AKit",
            dependencies: [.product(name: "AFfi", package: "AFfi")],
            swiftSettings: [.defaultIsolation(MainActor.self)]
        ),
        .testTarget(
            name: "AKitTests",
            dependencies: ["AKit"],
            swiftSettings: [.defaultIsolation(MainActor.self)]
        ),
    ]
)
