import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme
    @AppStorage(ThemeSetting.key) private var themeKey = RemoteTheme.default.key
    @AppStorage(Appearance.key) private var appearanceKey = Appearance.default.rawValue
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
                        Text(RemoteError.invalidHost.message).font(.caption).foregroundStyle(theme.error.color)
                    }
                } header: {
                    Text("RSI host")
                } footer: {
                    Text("The hub's Tailscale HTTPS name. Tailscale on this phone is the connection and its identity is the sign-in; nothing secret is stored.")
                }
                .themedRow()

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
                            .foregroundStyle((testFailed ? theme.error : theme.running).color)
                    }
                }
                .themedRow()

                Section {
                    ForEach(RemoteTheme.all) { option in
                        ThemeOptionRow(option: option, selected: option.key == theme.key) {
                            themeKey = option.key
                        }
                    }
                } header: {
                    Text("Theme")
                } footer: {
                    Text("The rsi TUI's themes. Goth is the TUI's default.")
                }
                .themedRow()

                Section {
                    Picker("Appearance", selection: $appearanceKey) {
                        ForEach(Appearance.allCases) { Text($0.label).tag($0.rawValue) }
                    }
                    .pickerStyle(.segmented)
                } header: {
                    Text("Appearance")
                } footer: {
                    Text("Dark is the TUI look. Light is a daytime variant of the chosen theme.")
                }
                .themedRow()

                Section("About") {
                    LabeledContent("App", value: AppInfo.version)
                    LabeledContent("Contracts", value: AppInfo.contract)
                }
                .themedRow()
            }
            .themedList()
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

/// One theme in the Settings picker: its name, primary and loader spectrum.
struct ThemeOptionRow: View {
    let option: RemoteTheme
    let selected: Bool
    let choose: () -> Void

    var body: some View {
        Button(action: choose) {
            HStack(spacing: 12) {
                Circle().fill(option.primary.color).frame(width: 14, height: 14)
                VStack(alignment: .leading, spacing: 4) {
                    Text(option.displayName)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(option.primaryText.color)
                    SpectrumStrip(theme: option)
                }
                Spacer(minLength: 8)
                if selected {
                    Image(systemName: "checkmark").foregroundStyle(option.primary.color)
                }
            }
            .padding(.vertical, 6)
            .padding(.horizontal, 10)
            .background(option.canvas.color, in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
