import SwiftUI
import Observation
import UniformTypeIdentifiers

struct MobileThemeVariant: Codable, Identifiable, Equatable {
    let id: String
    let name: String
    let appearance: String
    let recommendedSurfaceTreatment: String
    let colors: [String: String]
    let accent: [String: JSONValue]
    let syntax: [String: String]
    let presets: [String: [String: JSONValue]]?
}
struct MobileThemeFamily: Codable, Identifiable, Equatable {
    let id: String
    let name: String
    let variants: [MobileThemeVariant]
}
struct MobileThemeCatalog: Codable { let families: [MobileThemeFamily] }

@Observable
final class AppearanceSettings {
    static let shared = AppearanceSettings()

    /// `AccentPreset` names the desktop catalog can carry roles for, in picker
    /// order. The desktop's `zeron` preset is surfaced as Noches.
    static let presetNames = ["zeron", "orange", "amber", "green", "cyan", "blue", "pink"]
    /// The scalar roles every resolved variant carries (`AccentRoles`).
    static let accentRoleKeys = ["primary", "strong", "wash", "on", "activity", "selection", "caret"]
    private static let customKey = "appearance.custom"

    var mode: String { didSet { save(mode, "mode") } }
    var light: String { didSet { save(light, "light") } }
    var dark: String { didSet { save(dark, "dark") } }
    var accent: String { didSet { save(accent, "accent") } }
    var surface: String { didSet { save(surface, "surface") } }
    var systemDark = true
    @ObservationIgnored private let defaults: UserDefaults
    /// Palette keys the bundled catalog resolves. Imports must cover all of
    /// them before the renderer can consume the family.
    @ObservationIgnored private let paletteKeys: Set<String>
    let catalog: MobileThemeCatalog
    var custom: [MobileThemeFamily] = []
    var variants: [MobileThemeVariant] { (catalog.families + custom).flatMap(\.variants) }
    var isDark: Bool { mode == "dark" || (mode == "system" && systemDark) }
    var scheme: ColorScheme? { mode == "system" ? nil : (mode == "dark" ? .dark : .light) }
    /// Variant resolution mirrors the desktop registry's `resolve`: the stored
    /// selection first, then the built-in Zeron variant for that appearance,
    /// then any variant of that appearance.
    func variant(id: String, appearance: String) -> MobileThemeVariant {
        let builtIn = appearance == "dark" ? "zeron-dark" : "zeron-light"
        return variants.first { $0.id == id && $0.appearance == appearance }
            ?? variants.first { $0.id == builtIn }
            ?? variants.first { $0.appearance == appearance }
            ?? variants[0] // An empty catalog is a packaging error; see `bundledCatalog`.
    }
    var current: MobileThemeVariant { variant(id: isDark ? dark : light, appearance: isDark ? "dark" : "light") }
    var frosted: Bool { surface == "frosted" || (surface == "themeDefault" && current.recommendedSurfaceTreatment == "frosted") }
    var accents: [String: JSONValue] {
        // Preset roles always come from the desktop derivation
        // (`AccentRoles::derive`) keyed off the resolved variant, so an imported
        // family cannot paint roles the desktop would not produce. Anything that
        // is not a preset keeps the variant's own accent roles.
        let background = current.colors["background"] ?? "#000000"
        return ThemeRGBA.roles(preset: accent, dark: current.appearance == "dark", background: background) ?? current.accent
    }

    convenience init(defaults: UserDefaults = .standard) { self.init(defaults: defaults, catalog: Self.bundledCatalog()) }

    /// Designated initializer. `catalog` is injectable so tests can exercise
    /// import and repair without the app bundle.
    init(defaults: UserDefaults, catalog: MobileThemeCatalog) {
        self.defaults = defaults
        self.catalog = catalog
        paletteKeys = Set(catalog.families.flatMap(\.variants).flatMap(\.colors.keys))
        func read(_ key: String, _ fallback: String) -> String { defaults.string(forKey: "appearance." + key) ?? fallback }
        mode = read("mode", "system")
        light = read("light", "zeron-light")
        dark = read("dark", "zeron-dark")
        accent = read("accent", "themeDefault")
        surface = read("surface", "themeDefault")
        // Persisted customs are re-validated on every load: a blob written by
        // another build (or hand-edited defaults) must not inject an
        // unresolvable palette or a duplicate variant id into `variants`.
        let builtInIDs = Set(catalog.families.flatMap(\.variants).map(\.id))
        if let data = defaults.data(forKey: Self.customKey) {
            let stored = try? JSONDecoder().decode([MobileThemeFamily].self, from: data)
            let repaired = Self.repairedCustoms(stored ?? [], paletteKeys: paletteKeys, reservedIDs: builtInIDs)
            custom = repaired.families
            // Rewrite only when the stored blob was unreadable or repaired, so a
            // normal launch never touches the installed set.
            if stored == nil || repaired.repaired { Self.persist(custom, to: defaults) }
        }
        repairPreferences()
    }

    private func save(_ value: String, _ key: String) { defaults.set(value, forKey: "appearance." + key) }
    func color(_ key: String) -> Color { Self.color(current.colors[key] ?? current.colors["text"] ?? "#000000") }
    func accentColor(_ key: String) -> Color { Self.color(accents[key]?.stringValue ?? current.colors["text"] ?? "#000000") }

    /// Bundled export of the desktop catalog
    /// (`cargo run -q -p zeron-theme --example export-ios`). A missing or
    /// malformed catalog is a packaging error.
    static func bundledCatalog() -> MobileThemeCatalog {
        let url = Bundle.main.url(forResource: "DesktopThemes", withExtension: "json")!
        return try! JSONDecoder().decode(MobileThemeCatalog.self, from: Data(contentsOf: url))
    }

    static func isHexColor(_ text: String) -> Bool {
        text.range(of: "^#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?$", options: .regularExpression) != nil
    }

    /// Validate accent roles the way `AccentRoles` is serialized: the seven
    /// scalar roles must be hex colors and `glyph`, when present, must be a
    /// three-color ramp. Unknown keys and a malformed ramp are dropped so
    /// consumers fall back instead of painting an unresolved role.
    static func sanitizedAccentRoles(_ roles: [String: JSONValue]) -> [String: JSONValue]? {
        guard accentRoleKeys.allSatisfy({ isHexColor(roles[$0]?.stringValue ?? "") }) else { return nil }
        var sanitized: [String: JSONValue] = [:]
        for key in accentRoleKeys { sanitized[key] = roles[key] }
        if let glyph = roles["glyph"]?.arrayValue, glyph.count == 3, glyph.allSatisfy({ isHexColor($0.stringValue ?? "") }) {
            sanitized["glyph"] = .array(glyph)
        }
        return sanitized
    }

    /// A resolved family the renderer can consume: complete palette, well-formed
    /// accent roles, unique non-empty ids. Unknown or malformed accent presets
    /// are dropped rather than trusted; `nil` means the family is unusable.
    static func validated(_ family: MobileThemeFamily, paletteKeys: Set<String>, reservedIDs: Set<String>) -> MobileThemeFamily? {
        guard !family.id.isEmpty, !family.name.isEmpty, !family.variants.isEmpty,
              Set(family.variants.map(\.id)).count == family.variants.count else { return nil }
        var variants: [MobileThemeVariant] = []
        for variant in family.variants {
            guard !variant.id.isEmpty, !reservedIDs.contains(variant.id),
                  ["light", "dark"].contains(variant.appearance),
                  ["opaque", "frosted"].contains(variant.recommendedSurfaceTreatment),
                  paletteKeys.isSubset(of: Set(variant.colors.keys)),
                  variant.colors.values.allSatisfy(isHexColor),
                  variant.syntax.values.allSatisfy(isHexColor),
                  let accent = sanitizedAccentRoles(variant.accent) else { return nil }
            let presets = (variant.presets ?? [:]).reduce(into: [String: [String: JSONValue]]()) { result, entry in
                guard presetNames.contains(entry.key), let roles = sanitizedAccentRoles(entry.value) else { return }
                result[entry.key] = roles
            }
            variants.append(MobileThemeVariant(
                id: variant.id, name: variant.name, appearance: variant.appearance,
                recommendedSurfaceTreatment: variant.recommendedSurfaceTreatment,
                colors: variant.colors, accent: accent,
                syntax: variant.syntax, presets: presets.isEmpty ? nil : presets))
        }
        return MobileThemeFamily(id: family.id, name: family.name, variants: variants)
    }

    /// Customs read back from `UserDefaults`, repaired against the catalog:
    /// invalid families, duplicate family ids, and ids already reserved by a
    /// built-in or an earlier family are dropped. `repaired` reports whether the
    /// stored blob differs from what was loaded and must be rewritten.
    static func repairedCustoms(_ stored: [MobileThemeFamily], paletteKeys: Set<String>, reservedIDs: Set<String>)
        -> (families: [MobileThemeFamily], repaired: Bool) {
        var families: [MobileThemeFamily] = []
        var reserved = reservedIDs
        var repaired = false
        for family in stored {
            guard !families.contains(where: { $0.id == family.id }),
                  let valid = validated(family, paletteKeys: paletteKeys, reservedIDs: reserved) else {
                repaired = true
                continue
            }
            reserved.formUnion(valid.variants.map(\.id))
            families.append(valid)
            if valid != family { repaired = true }
        }
        return (families, repaired)
    }

    /// Stored selections with invalid values replaced by defaults, so a value
    /// from an older build, a hand-edited default, or a variant removed with its
    /// family never leaves a picker without a selection.
    static func repaired(mode: String, light: String, dark: String, accent: String, surface: String,
                         variants: [MobileThemeVariant]) -> (mode: String, light: String, dark: String, accent: String, surface: String) {
        let lightIDs = variants.filter { $0.appearance == "light" }.map(\.id)
        let darkIDs = variants.filter { $0.appearance == "dark" }.map(\.id)
        return (
            ["system", "light", "dark"].contains(mode) ? mode : "system",
            lightIDs.contains(light) ? light : lightIDs.first { $0 == "zeron-light" } ?? lightIDs.first ?? "zeron-light",
            darkIDs.contains(dark) ? dark : darkIDs.first { $0 == "zeron-dark" } ?? darkIDs.first ?? "zeron-dark",
            accent == "themeDefault" || presetNames.contains(accent) ? accent : "themeDefault",
            ["themeDefault", "frosted", "opaque"].contains(surface) ? surface : "themeDefault")
    }

    /// Repair and persist the stored selections (the `didSet` hooks write the
    /// repaired value back).
    func repairPreferences() {
        let repaired = Self.repaired(mode: mode, light: light, dark: dark, accent: accent, surface: surface, variants: variants)
        if repaired.mode != mode { mode = repaired.mode }
        if repaired.light != light { light = repaired.light }
        if repaired.dark != dark { dark = repaired.dark }
        if repaired.accent != accent { accent = repaired.accent }
        if repaired.surface != surface { surface = repaired.surface }
    }

    private static func persist(_ families: [MobileThemeFamily], to defaults: UserDefaults) {
        guard let data = try? JSONEncoder().encode(families) else { return }
        defaults.set(data, forKey: customKey)
    }

    static func color(_ hex: String) -> Color {
        let text = String(hex.dropFirst())
        guard let value = UInt64(text, radix: 16), text.count == 6 || text.count == 8 else { return .clear }
        let n = text.count == 6 ? value << 8 | 255 : value
        return Color(.sRGB, red: Double((n >> 24) & 255) / 255,
                     green: Double((n >> 16) & 255) / 255, blue: Double((n >> 8) & 255) / 255,
                     opacity: Double(n & 255) / 255)
    }

    /// Install a resolved theme family exported by the desktop theme tool.
    /// Validation mirrors the desktop registry's contract for a resolved
    /// family; optional data (accent presets, the glyph ramp) is sanitized
    /// rather than trusted, and selections that pointed into a replaced family
    /// are repaired.
    func importFamily(_ data: Data) throws {
        guard data.count <= 16 * 1024 * 1024 else { throw RelayError.rpc("Theme files must be smaller than 16 MB.") }
        guard let decoded = try? JSONDecoder().decode(MobileThemeFamily.self, from: data) else {
            throw RelayError.rpc("Import a resolved theme family exported by the desktop theme tool.")
        }
        guard !catalog.families.contains(where: { $0.id == decoded.id }) else {
            throw RelayError.rpc("A built-in theme family already uses this ID.")
        }
        let builtInIDs = Set(catalog.families.flatMap(\.variants).map(\.id))
        guard let family = Self.validated(decoded, paletteKeys: paletteKeys, reservedIDs: builtInIDs) else {
            throw RelayError.rpc("Import a resolved theme family exported by the desktop theme tool.")
        }
        var updated = custom.filter { $0.id != family.id }
        let otherIDs = Set(updated.flatMap(\.variants).map(\.id))
        guard family.variants.allSatisfy({ !otherIDs.contains($0.id) }) else { throw RelayError.rpc("A theme with this ID is already installed.") }
        updated.append(family)
        Self.persist(updated, to: defaults)
        custom = updated
        repairPreferences()
    }
}

struct AppearanceSettingsView: View {
    @Bindable private var settings = AppearanceSettings.shared
    @State private var importing = false
    @State private var error: String?
    var body: some View {
        Form {
            Section("Appearance") {
                Picker("Appearance", selection: $settings.mode) {
                    Text("System").tag("system"); Text("Light").tag("light"); Text("Dark").tag("dark")
                }
                themePicker("Light theme", appearance: "light", selection: $settings.light)
                themePicker("Dark theme", appearance: "dark", selection: $settings.dark)
            }
            .listRowBackground(Theme.surfaceRaised)
            Section("Color and surfaces") {
                Picker("Accent", selection: $settings.accent) {
                    Text("Theme default").tag("themeDefault")
                    ForEach(AppearanceSettings.presetNames, id: \.self) {
                        Text($0 == "zeron" ? "Noches" : $0.capitalized).tag($0)
                    }
                }
                Picker("Surfaces", selection: $settings.surface) {
                    Text("Theme default").tag("themeDefault")
                    Text("Frosted").tag("frosted"); Text("Opaque").tag("opaque")
                }
            }
            .listRowBackground(Theme.surfaceRaised)
            Section {
                VStack(alignment: .leading, spacing: 12) {
                    Label("Your agents, on your computer", systemImage: "desktopcomputer")
                        .font(Theme.sans(18, weight: .semibold)).foregroundStyle(Theme.text)
                    Text("Geist for conversation. Geist Mono for code.")
                        .font(Theme.sans(14)).foregroundStyle(Theme.textMuted)
                    Text("noches · connected").font(Theme.mono(13)).foregroundStyle(Theme.accent)
                }.padding(.vertical, 12)
            }.listRowBackground(Theme.surfaceRaised)
            Section {
                Button("Import desktop theme…") { importing = true }
                if let error { Text(error).foregroundStyle(Theme.danger) }
            } footer: {
                Text("Uses the desktop's theme catalog and accent colors. Import a resolved theme family JSON file to add a custom theme.")
            }
            .listRowBackground(Theme.surfaceRaised)
        }
        .font(Theme.sans(16))
        .foregroundStyle(Theme.text)
        .scrollContentBackground(.hidden).background(Theme.bg)
        .navigationTitle("Appearance")
        .fileImporter(isPresented: $importing, allowedContentTypes: [.json]) { result in
            do {
                let url = try result.get()
                let access = url.startAccessingSecurityScopedResource()
                defer { if access { url.stopAccessingSecurityScopedResource() } }
                try settings.importFamily(Data(contentsOf: url))
                error = nil
            } catch { self.error = error.localizedDescription }
        }
    }
    private func themePicker(_ title: String, appearance: String, selection: Binding<String>) -> some View {
        Picker(title, selection: selection) {
            ForEach(settings.variants.filter { $0.appearance == appearance }) { variant in
                Text(variant.name.replacingOccurrences(of: "Zeron", with: "Noches")).tag(variant.id)
            }
        }
    }
}

struct CompanionPanel: ViewModifier {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    func body(content: Content) -> some View {
        if AppearanceSettings.shared.frosted && !reduceTransparency {
            content.background(Theme.surface.opacity(0.8))
                .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 24))
                .clipShape(RoundedRectangle(cornerRadius: 24))
        } else {
            content.background(Theme.surface, in: RoundedRectangle(cornerRadius: 24))
        }
    }
}

private struct ThemeGlassModifier<S: Shape>: ViewModifier {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    let glass: Glass
    let shape: S
    @ViewBuilder func body(content: Content) -> some View {
        if AppearanceSettings.shared.frosted && !reduceTransparency {
            content.glassEffect(glass, in: shape)
        } else {
            content.background(Theme.surfaceRaised, in: shape)
                .overlay(shape.stroke(Theme.border, lineWidth: 1))
        }
    }
}

extension View {
    func nochesGlass<S: Shape>(_ glass: Glass = .regular, in shape: S) -> some View {
        modifier(ThemeGlassModifier(glass: glass, shape: shape))
    }
}

/// The desktop's AccentRoles::derive arithmetic, including byte rounding and
/// contrast correction. Built-ins use exported roles; imported families use this.
struct ThemeRGBA {
    var r: Double; var g: Double; var b: Double; var a: Double = 255
    init(_ hex: String) {
        let n = UInt64(hex.dropFirst(), radix: 16) ?? 0
        let value = hex.count == 7 ? n << 8 | 255 : n
        r = Double((value >> 24) & 255); g = Double((value >> 16) & 255)
        b = Double((value >> 8) & 255); a = Double(value & 255)
    }
    init(r: Double, g: Double, b: Double, a: Double = 255) { self.r = r; self.g = g; self.b = b; self.a = a }
    var hex: String { String(format: "#%02x%02x%02x%02x", Int(r), Int(g), Int(b), Int(a)) }
    func mix(_ other: Self, _ amount: Double) -> Self {
        Self(r: (r + (other.r-r)*amount).rounded(), g: (g + (other.g-g)*amount).rounded(),
             b: (b + (other.b-b)*amount).rounded(), a: (a + (other.a-a)*amount).rounded())
    }
    func alpha(_ value: Double) -> Self { Self(r: r, g: g, b: b, a: (value*255).rounded()) }
    var luminance: Double {
        func linear(_ v: Double) -> Double { let x = v/255; return x <= 0.04045 ? x/12.92 : pow((x+0.055)/1.055,2.4) }
        return 0.2126*linear(r) + 0.7152*linear(g) + 0.0722*linear(b)
    }
    func contrast(_ background: Self) -> Double {
        let foreground = a == 255 ? self : background.mix(self, a/255)
        return (max(foreground.luminance,background.luminance)+0.05)/(min(foreground.luminance,background.luminance)+0.05)
    }
    func ensuring(_ background: Self, _ minimum: Double) -> Self {
        if contrast(background) >= minimum { return self }
        let black = Self("#000000"), white = Self("#ffffff")
        let target = black.contrast(background) >= white.contrast(background) ? black : white
        for step in 1...20 {
            let candidate = mix(target, Double(step)/20)
            if candidate.contrast(background) >= minimum { return candidate }
        }
        return target
    }
    static func roles(preset: String, dark: Bool, background: String) -> [String: JSONValue]? {
        let seeds = ["zeron": ["#8b7cf6", "#5b43e8"], "orange": ["#fb923c", "#c2410c"],
                     "amber": ["#fbbf24", "#a16207"], "green": ["#4ade80", "#15803d"],
                     "cyan": ["#22d3ee", "#0e7490"], "blue": ["#60a5fa", "#2563eb"], "pink": ["#f472b6", "#be185d"]]
        guard let seed = seeds[preset] else { return nil }
        let bg = Self(background), white = Self("#ffffff"), black = Self("#000000")
        let primary = Self(seed[dark ? 0 : 1]).ensuring(bg, 3)
        let on = white.contrast(primary) >= black.contrast(primary) ? white : black
        let strong = on.contrast(primary) < 4.5 ? primary.ensuring(on, 4.5) : primary
        return ["primary": .string(primary.hex), "strong": .string(strong.hex), "on": .string(on.hex),
                "wash": .string(primary.alpha(dark ? 0.22 : 0.12).hex), "selection": .string(primary.alpha(dark ? 0.35 : 0.24).hex),
                "caret": .string(primary.hex), "activity": .string(primary.hex),
                "glyph": .array([.string(primary.mix(dark ? white : bg, dark ? 0.28 : 0.18).hex),
                                  .string(primary.hex), .string(primary.mix(black, dark ? 0.18 : 0.26).hex)])]
    }
}
