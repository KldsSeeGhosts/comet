// Semantic theme roles resolved from the desktop theme catalog.
// Built-in colors and accents are exported from zeron-theme. Layout metrics
// stay independent from the selected palette; UIKit and SwiftUI share roles.

import SwiftUI

enum Theme {
    static func wash(_ alpha: Double) -> Color {
        (AppearanceSettings.shared.isDark ? Color.white : Color.black).opacity(alpha)
    }
    // ---- paint: surfaces ----
    /// Main content background.
    static var bg: Color { AppearanceSettings.shared.color("background") }
    /// Shell / sidebar surface.
    static var surface: Color { AppearanceSettings.shared.color("shell") }
    /// Raised surface: popovers, dialogs, cards.
    static var surfaceRaised: Color { AppearanceSettings.shared.color("raised") }
    /// Hover/pressed wash for interactive rows (white, low alpha).
    static var elementHover: Color { AppearanceSettings.shared.color("hover") }
    /// Active/selected wash.
    static var elementActive: Color { AppearanceSettings.shared.color("active") }
    /// Theme-authored hairline border.
    static var border: Color { AppearanceSettings.shared.color("border") }
    /// Stronger border for focused/raised edges.
    static var borderStrong: Color { AppearanceSettings.shared.color("borderStrong") }

    // ---- paint: text ----
    static var text: Color { AppearanceSettings.shared.color("text") }
    static var textMuted: Color { AppearanceSettings.shared.color("textMuted") }
    static var textFaint: Color { AppearanceSettings.shared.color("textFaint") }

    // ---- paint: accents ----
    static var accent: Color { AppearanceSettings.shared.accentColor("primary") }
    static var accentStrong: Color { AppearanceSettings.shared.accentColor("strong") }
    static var danger: Color { AppearanceSettings.shared.color("danger") }
    static var dangerSoft: Color { AppearanceSettings.shared.color("dangerMuted") }
    static var warning: Color { AppearanceSettings.shared.color("warning") }

    // ---- paint: status dots (shell/spaces.rs status_dot_color) ----
    static var statusWorking: Color { AppearanceSettings.shared.accentColor("activity") }
    static var statusCompleted: Color { AppearanceSettings.shared.color("success") }
    /// Claude brand orange — kept even on the mono surface.
    static let claudeBrand = Color(red: 0xD9 / 255.0, green: 0x77 / 255.0, blue: 0x57 / 255.0)

    // ---- paint: markdown inline code ----
    static var inlineCodeText: Color { AppearanceSettings.color(AppearanceSettings.shared.current.syntax["stringSpecial"] ?? "#8b7cf6") }
    static var inlineCodeWash: Color { inlineCodeText.opacity(0.12) }

    // ---- paint: syntax tokens (soft, paint-only) ----
    static var tokenKeyword: Color { AppearanceSettings.color(AppearanceSettings.shared.current.syntax["keyword"] ?? "#8b7cf6") }
    static var tokenString: Color { AppearanceSettings.color(AppearanceSettings.shared.current.syntax["string"] ?? "#8b7cf6") }
    static var tokenNumber: Color { AppearanceSettings.color(AppearanceSettings.shared.current.syntax["number"] ?? "#8b7cf6") }

    // ---- numbers drive layout (pt) ----
    static let bubbleRadius: CGFloat = 22
    static let panelRadius: CGFloat = 10
    static let controlRadius: CGFloat = 6
    static let spaceXS: CGFloat = 4
    static let spaceSM: CGFloat = 8
    static let spaceMD: CGFloat = 12
    static let spaceLG: CGFloat = 16
}

// MARK: - Fonts

extension Theme {
    static let fontSansName = "Geist"
    static let fontMonoName = "GeistMono-Regular"

    static func sans(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        // Static weight cuts register as separate families — select by
        // PostScript name so weights actually resolve.
        let name: String
        if weight == .medium {
            name = "Geist-Medium"
        } else if weight == .semibold {
            name = "Geist-SemiBold"
        } else if weight == .bold {
            name = "Geist-Bold"
        } else {
            name = "Geist-Regular"
        }
        return .custom(name, size: size)
    }

    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .custom(fontMonoName, size: size).weight(weight)
    }

    static func sansUI(_ size: CGFloat, weight: UIFont.Weight = .regular) -> UIFont {
        let traits: [UIFontDescriptor.TraitKey: Any] = [.weight: weight]
        let descriptor = UIFontDescriptor(fontAttributes: [
            .family: "Geist",
            .traits: traits,
        ])
        return UIFont(descriptor: descriptor, size: size)
    }

    static func monoUI(_ size: CGFloat) -> UIFont {
        UIFont(name: fontMonoName, size: size)
            ?? .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

// MARK: - Color primitives (ported from theme.rs)

/// A neutral (chroma 0) oklch tone. Chroma 0 means r == g == b exactly.
func neutral(_ lightness: Double) -> Color {
    let v = Double(oklchToSrgb(l: lightness, c: 0, hDeg: 0)[0])
    return Color(red: v, green: v, blue: v)
}

/// White at the given alpha — the hairline/wash primitive.
func whiteAlpha(_ alpha: Double) -> Color {
    Color.white.opacity(alpha)
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ #0d0d0d).
func grey(_ value: UInt8) -> Color {
    let v = Double(value) / 255.0
    return Color(red: v, green: v, blue: v)
}

/// oklch (CSS notation: L 0..1, C, H degrees) → sRGB Color.
func oklch(_ l: Double, _ c: Double, _ hDeg: Double) -> Color {
    let rgb = oklchToSrgb(l: l, c: c, hDeg: hDeg)
    return Color(red: Double(rgb[0]), green: Double(rgb[1]), blue: Double(rgb[2]))
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
func oklchToSrgb(l: Double, c: Double, hDeg: Double) -> [Double] {
    let h = hDeg * .pi / 180
    let a = c * cos(h)
    let b = c * sin(h)

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.39633778 * a + 0.21580376 * b
    let m_ = l - 0.105561346 * a - 0.06385417 * b
    let s_ = l - 0.08948418 * a - 1.2914855 * b
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_)

    // LMS → linear sRGB
    let r = 4.0767417 * l3 - 3.3077116 * m3 + 0.23096993 * s3
    let g = -1.268438 * l3 + 2.6097574 * m3 - 0.3413194 * s3
    let bl = -0.0041960863 * l3 - 0.7034186 * m3 + 1.7076147 * s3

    return [gammaEncode(r), gammaEncode(g), gammaEncode(bl)]
}

private func gammaEncode(_ x: Double) -> Double {
    let x = min(max(x, 0), 1)
    return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055
}
