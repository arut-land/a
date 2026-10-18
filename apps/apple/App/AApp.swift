import AKit
import SwiftUI

@main
struct AApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup("a") {
            ContentView(model: model)
                .frame(minWidth: 760, minHeight: 480)
        }
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Session…") { model.chooseWorkspace() }
                    .keyboardShortcut("n")
            }
            CommandGroup(after: .appSettings) {
                Button("Install Command Line Tool") {
                    Task { await model.installCli(replace: false, announce: true) }
                }
            }
        }
        Settings {
            SettingsView(model: model)
        }
    }
}

/// The app's connection to the per-user daemon and the sessions it shows,
/// one store each.
@Observable
final class AppModel {
    private(set) var client: Client?
    private(set) var list: SessionListStore?
    /// Open sessions by id.
    private(set) var stores: [String: SessionStore] = [:]
    /// A new session, until the daemon names it.
    private(set) var draft: SessionStore?
    var selection: String?
    var problem: String?
    /// Where "Install Command Line Tool" links `a`.
    var cliDir = FileManager.default.homeDirectoryForCurrentUser.appending(path: ".local/bin")
    private(set) var cliStatus: CliStatus?
    private(set) var cliProblem: String?
    /// A message for an alert, such as the outcome of installing the CLI.
    var notice: String?

    init() {
        // The bundled `a` (Contents/Helpers), staged like any entry point so
        // the app and the CLI share one daemon and its sessions.
        let helper = Bundle.main.bundleURL.appending(path: "Contents/Helpers/a").path
        do {
            let client = try Client(helper: helper)
            self.client = client
            list = SessionListStore(client: client)
        } catch {
            problem = error.localizedDescription
        }
    }

    /// Attaches to session `id`, once: its store keeps following it.
    func open(_ id: String) {
        guard stores[id] == nil, let client else { return }
        let store = SessionStore(session: Session(client: client))
        stores[id] = store
        Task { await store.open(.id(id: id)) }
    }

    func newSession(in workspace: URL) {
        guard let client else { return }
        let store = SessionStore(session: Session(client: client))
        draft = store
        selection = nil
        Task { await store.open(.new(workspace: workspace.path)) }
    }

    /// Files the draft under its id once it is attached.
    func adoptDraft() {
        guard let draft, let id = draft.sessionId else { return }
        stores[id] = draft
        self.draft = nil
        selection = id
        Task { await list?.refresh() }
    }

    func chooseWorkspace() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.prompt = "Start Session"
        panel.message = "Choose the workspace the new session works in."
        if panel.runModal() == .OK, let url = panel.url {
            newSession(in: url)
        }
    }

    var daemonPid: UInt32? { client?.daemonPid() }

    func refreshCli() async {
        guard let client else { return }
        do {
            cliStatus = try await client.cliStatus(dir: cliDir.path, search: nil)
            cliProblem = nil
        } catch {
            cliProblem = error.localizedDescription
        }
    }

    /// Links `a` in `cliDir` to the daemon's build, unless an `a` is on the
    /// `PATH` already (a link an earlier install made is replaced only with
    /// `replace`). `announce` tells the outcome in an alert.
    func installCli(replace: Bool, announce: Bool = false) async {
        guard let client else { return }
        do {
            let status = try await client.installCli(dir: cliDir.path, search: nil, replace: replace)
            (cliStatus, cliProblem) = (status, nil)
            if announce, case .installed(let path) = status {
                notice = "The a command is installed at \(path)."
            }
        } catch {
            cliProblem = error.localizedDescription
            if announce { notice = error.localizedDescription }
        }
    }

    func chooseCliDir() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.directoryURL = cliDir
        panel.prompt = "Choose"
        panel.message = "Choose a directory on your PATH for the a command."
        if panel.runModal() == .OK, let url = panel.url {
            cliDir = url
            Task { await refreshCli() }
        }
    }

    /// `-AOpen latest` or `-AOpen <id or short id>` selects a session at
    /// launch.
    func openRequested() {
        guard selection == nil, let wanted = UserDefaults.standard.string(forKey: "AOpen"),
            let sessions = list?.sessions, !sessions.isEmpty
        else { return }
        let found =
            wanted == "latest"
            ? sessions.first : sessions.first { $0.id.hasPrefix(wanted) || $0.short == wanted }
        selection = found?.id
    }
}
