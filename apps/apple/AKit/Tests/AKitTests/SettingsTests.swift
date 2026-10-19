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

/// The settings pane's round trip through the daemon, the one writer of
/// `$A_HOME/config.toml`, on the throwaway home `mise run apple:test` gives
/// (never the default one).
@Test(.enabled(if: live != nil))
func settingsRoundTripThroughTheDaemon() async throws {
    let client = try Client(helper: live!)
    let home = ProcessInfo.processInfo.environment["A_HOME"]!
    #expect(client.configFile() == URL(filePath: home).appending(path: "config.toml").path)
    let before = try await client.settings()

    let wanted = Settings(
        model: "openai:gpt-test", agent: before.agent,
        env: [
            EnvVar(name: "OPENAI_API_KEY", value: "sk-\"quoted\"\nline"),
            EnvVar(name: "OPENAI_BASE_URL", value: "http://127.0.0.1:9/v1"),
        ])
    #expect(try await client.saveSettings(from: before, to: wanted) == wanted)
    #expect(try await client.settings() == wanted)
    let attributes = try FileManager.default.attributesOfItem(atPath: client.configFile())
    #expect((attributes[.posixPermissions] as? Int) == 0o600)

    // Another pane's save, from what it loaded before this one saved: only
    // its own edit lands, so neither undoes the other.
    var other = before
    other.env.append(EnvVar(name: "SECOND", value: "2"))
    let merged = try await client.saveSettings(from: before, to: other)
    #expect(merged.model == "openai:gpt-test")
    #expect(Set(merged.env.map(\.name)) == ["OPENAI_API_KEY", "OPENAI_BASE_URL", "SECOND"])

    await #expect(throws: ClientError.self) {
        try await client.saveSettings(
            from: merged,
            to: Settings(model: nil, agent: nil, env: [EnvVar(name: "NOT A NAME", value: "")]))
    }
    // Back to how it was, so other tests see their defaults.
    _ = try await client.saveSettings(from: merged, to: before)
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
