import AKit
import SwiftUI

/// The app's settings window (⌘,): the model and provider variables in
/// `$A_HOME/config.toml`, and the command line tool.
struct SettingsView: View {
    let model: AppModel

    var body: some View {
        TabView {
            Tab("Model", systemImage: "cpu") { ModelSettings(model: model) }
            Tab("Command Line", systemImage: "terminal") { CommandLineSettings(model: model) }
        }
        .frame(width: 560)
        .fixedSize(horizontal: false, vertical: true)
    }
}

/// One editable `[env]` row.
struct VarRow: Identifiable {
    let id = UUID()
    var name: String
    var value: String
}

struct ModelSettings: View {
    let model: AppModel
    @State private var modelName = ""
    @State private var agent: String?
    @State private var vars: [VarRow] = []
    /// The settings as last loaded or saved: a save sends only the edit
    /// made since, so it keeps what another client saved meanwhile.
    @State private var loaded: AFfi.Settings?
    @State private var problem: String?
    @State private var saved = false

    var body: some View {
        Form {
            Section {
                TextField("Model", text: $modelName, prompt: Text("openai:gpt-6-luna"))
                    .font(.body.monospaced())
            } footer: {
                Text("host:model, as in openai:gpt-6-luna. Empty uses the default.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Provider variables") {
                ForEach($vars) { $row in
                    HStack {
                        TextField("Name", text: $row.name, prompt: Text("NAME"))
                            .font(.body.monospaced())
                            .frame(width: 180)
                        // Masked by the daemon's rule for what it keeps out of its logs.
                        if model.client?.isSecretName(name: row.name) == true {
                            SecureField("Value", text: $row.value, prompt: Text("value"))
                        } else {
                            TextField("Value", text: $row.value, prompt: Text("value"))
                        }
                        Button("Remove", systemImage: "minus.circle") {
                            vars.removeAll { $0.id == row.id }
                        }
                        .labelStyle(.iconOnly).buttonStyle(.borderless)
                    }
                    .labelsHidden()
                }
                HStack {
                    Button("Add Variable", systemImage: "plus") {
                        vars.append(VarRow(name: "", value: ""))
                    }
                    ForEach(["OPENAI_API_KEY", "OPENAI_BASE_URL"], id: \.self) { name in
                        if !vars.contains(where: { $0.name == name }) {
                            Button(name) { vars.append(VarRow(name: name, value: "")) }
                                .font(.caption.monospaced())
                        }
                    }
                }
                .buttonStyle(.borderless)
            }
            Section {
                HStack {
                    if let problem {
                        Label(problem, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.red).font(.caption)
                    } else if saved {
                        Label("Saved", systemImage: "checkmark").foregroundStyle(.secondary).font(.caption)
                    }
                    Spacer()
                    Button("Revert") { Task { await load() } }
                    Button("Save") { Task { await save() } }
                        .keyboardShortcut("s").buttonStyle(.borderedProminent)
                }
            } footer: {
                Text("Saved by the daemon to \(model.client?.configFile() ?? "config.toml"), readable only by you. Sessions use these when their shell does not set them, from their next model call on.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .task { await load() }
    }

    private func show(_ settings: AFfi.Settings) {
        loaded = settings
        modelName = settings.model ?? ""
        agent = settings.agent
        vars = settings.env.map { VarRow(name: $0.name, value: $0.value) }
    }

    private func load() async {
        guard let client = model.client else { return }
        do {
            show(try await client.settings())
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        saved = false
    }

    private func save() async {
        guard let client = model.client, let loaded else { return }
        let env = vars.map { EnvVar(name: $0.name, value: $0.value) }
        do {
            show(try await client.saveSettings(
                from: loaded, to: AFfi.Settings(model: modelName, agent: agent, env: env)))
            problem = nil
            saved = true
        } catch {
            problem = error.localizedDescription
        }
    }
}

struct CommandLineSettings: View {
    let model: AppModel

    var body: some View {
        Form {
            Section("Command line tool") {
                LabeledContent("Install in") {
                    HStack {
                        Text(model.cliDir.path).font(.body.monospaced()).lineLimit(1)
                            .truncationMode(.head)
                        Button("Choose…") { model.chooseCliDir() }
                    }
                }
                CliStatusView(model: model)
            }
            Section("Daemon") {
                LabeledContent("Status") {
                    Text(model.daemonPid.map { "running, pid \(String($0))" } ?? "not running")
                }
                LabeledContent("Build") {
                    Text(model.client?.build() ?? "—").font(.caption.monospaced()).textSelection(.enabled)
                }
                LabeledContent("Log") {
                    Text(model.client?.daemonLog() ?? "—").font(.caption.monospaced())
                        .textSelection(.enabled).lineLimit(1).truncationMode(.head)
                }
            }
        }
        .formStyle(.grouped)
        .task { await model.refreshCli() }
    }
}

struct CliStatusView: View {
    let model: AppModel

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            switch model.cliStatus {
            case nil:
                ProgressView().controlSize(.small)
            case .installed(let path)?:
                Label("Installed at \(path); it runs the daemon's build.", systemImage: "checkmark.circle")
            case .stale(let path, let target)?:
                Label("\(path) links to another build (\(target)).", systemImage: "arrow.triangle.2.circlepath")
                Spacer()
                Button("Replace") { Task { await model.installCli(replace: true) } }
            case .foreign(let path)?:
                Label("\(path) is installed some other way; it is left alone.", systemImage: "info.circle")
            case .absent(let path, let onPath)?:
                VStack(alignment: .leading, spacing: 2) {
                    Label("Not installed. Installing links \(path).", systemImage: "terminal")
                    if !onPath {
                        Text("\(model.cliDir.path) is not on your PATH; add it in your shell's profile.")
                            .font(.caption).foregroundStyle(.orange)
                    }
                }
                Spacer()
                Button("Install") { Task { await model.installCli(replace: false) } }
            }
        }
        .font(.callout)
        if let problem = model.cliProblem {
            Text(problem).font(.caption).foregroundStyle(.red)
        }
    }
}
