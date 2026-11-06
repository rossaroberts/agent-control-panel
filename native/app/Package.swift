// swift-tools-version:6.0
// Fleet for the Mac: FleetKit (the wire, the board model and, later, the connections; no UI) and FleetApp (SwiftUI +
// AppKit). SwiftTerm is the only third-party package (Ruling P3-R2): pinned to an exact release at least two weeks old.
// FleetApp draws its terminals with it from Task 6 on.

import PackageDescription

let package = Package(
    name: "Fleet",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "FleetKit", targets: ["FleetKit"]),
        .executable(name: "FleetApp", targets: ["FleetApp"]),
    ],
    dependencies: [
        // v1.19.0, released 2026-08-18: the newest release without swift-png in its graph (Ruling P3-R4; 1.99.x adds it,
        // 1.20.0 is a prerelease).
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", exact: "1.19.0"),
    ],
    targets: [
        .target(name: "FleetKit"),
        .executableTarget(
            name: "FleetApp",
            dependencies: ["FleetKit", .product(name: "SwiftTerm", package: "SwiftTerm")]
        ),
        .testTarget(name: "FleetKitTests", dependencies: ["FleetKit"]),
    ]
)
