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
        // v1.99.1, released 2026-10-21.
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", exact: "1.99.1"),
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
