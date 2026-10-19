import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var hostText = ""
    @State private var hostRejected = false
    @State private var testing = false
    @State private var testResult: String?
    @State private var testFailed = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("hub.your-tailnet.ts.net", text: $hostText)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .submitLabel(.done)
                        .onSubmit(save)
                    Button("Save", action: save)
                    if hostRejected {
                        Text(RemoteError.invalidHost.message).font(.caption).foregroundStyle(.red)
                    }
                } header: {
                    Text("RSI host")
                } footer: {
                    Text("The hub's Tailscale HTTPS name. Tailscale on this phone is the connection and its identity is the sign-in; nothing secret is stored.")
                }

                Section("Connection") {
                    Button {
                        Task { await test() }
                    } label: {
                        HStack {
                            Text("Test connection")
                            if testing { Spacer(); ProgressView() }
                        }
                    }
                    .disabled(model.host == nil || testing)
                    if let testResult {
                        Text(testResult)
                            .font(.caption)
                            .foregroundStyle(testFailed ? .red : .green)
                    }
                }

                Section("About") {
                    LabeledContent("App", value: AppInfo.version)
                    LabeledContent("Contracts", value: AppInfo.contract)
                }
            }
            .navigationTitle("Settings")
            .onAppear { hostText = model.host ?? "" }
        }
    }

    private func save() {
        hostRejected = !model.setHost(hostText)
        if !hostRejected { hostText = model.host ?? hostText }
        testResult = nil
    }

    private func test() async {
        testing = true
        defer { testing = false }
        switch await model.testConnection() {
        case .success(let info):
            testFailed = false
            testResult = "Connected. Gateway protocol \(info.item.protocol)."
        case .failure(let error):
            testFailed = true
            testResult = error.message
        }
    }
}
