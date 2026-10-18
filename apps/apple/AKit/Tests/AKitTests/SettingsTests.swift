import Foundation
import Testing

@testable import AKit

/// A scratch directory for one test.
func scratch(_ name: String) throws -> URL {
    let dir = FileManager.default.temporaryDirectory
        .appending(path: "akit-\(name)-\(ProcessInfo.processInfo.processIdentifier)")
    try? FileManager.default.removeItem(at: dir)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

/// The settings pane's round trip through `$A_HOME/config.toml`, on the
/// throwaway home `mise run apple:test` gives (never the default one).
@Test(.enabled(if: live != nil))
func settingsRoundTripThroughAPrivateConfigFile() throws {
    let client = try Client(helper: live!)
    let home = ProcessInfo.processInfo.environment["A_HOME"]!
    let before = try client.settings()
    #expect(before.path == URL(filePath: home).appending(path: "config.toml").path)

    let wanted = Settings(
        model: "openai:gpt-test", agent: nil,
        env: [
            EnvVar(name: "OPENAI_API_KEY", value: "sk-\"quoted\"\nline"),
            EnvVar(name: "OPENAI_BASE_URL", value: "http://127.0.0.1:9/v1"),
        ],
        path: "")
    try client.saveSettings(settings: wanted)
    var read = try client.settings()
    read.path = ""
    #expect(read == wanted)
    let attributes = try FileManager.default.attributesOfItem(atPath: before.path)
    #expect((attributes[.posixPermissions] as? Int) == 0o600)

    #expect(throws: ClientError.self) {
        try client.saveSettings(
            settings: Settings(model: nil, agent: nil, env: [EnvVar(name: "NOT A NAME", value: "")], path: ""))
    }
    // Back to how it was, so other tests see their defaults.
    try client.saveSettings(settings: before)
}

@Test(.enabled(if: live != nil))
func installCliLinksTheStagedBuildOnlyWhereNoAIsFound() async throws {
    let client = try Client(helper: live!)
    let home = ProcessInfo.processInfo.environment["A_HOME"]!
    let dir = try scratch("install-cli")
    let bin = dir.appending(path: "bin")

    guard case .absent(let path, false) = try await client.cliStatus(dir: bin.path, search: "") else {
        Issue.record("expected nothing installed")
        return
    }
    let installed = try await client.installCli(dir: bin.path, search: "", replace: false)
    #expect(installed == .installed(path: path))
    let target = try FileManager.default.destinationOfSymbolicLink(atPath: path)
    #expect(target == URL(filePath: home).appending(path: "runtime/current/a").path)
    #expect(try await client.installCli(dir: bin.path, search: "", replace: false) == installed)

    // An `a` from elsewhere on the PATH wins, and is never touched.
    let brew = dir.appending(path: "brew")
    try FileManager.default.createDirectory(at: brew, withIntermediateDirectories: true)
    let foreign = brew.appending(path: "a")
    FileManager.default.createFile(atPath: foreign.path, contents: Data("#!/bin/sh\n".utf8),
        attributes: [.posixPermissions: 0o755])
    let other = dir.appending(path: "other")
    #expect(try await client.cliStatus(dir: other.path, search: brew.path) == .foreign(path: foreign.path))
    await #expect(throws: ClientError.self) {
        try await client.installCli(dir: other.path, search: brew.path, replace: true)
    }
    try FileManager.default.removeItem(at: dir)
}
