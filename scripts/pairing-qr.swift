#!/usr/bin/env swift
// Render a noches-connect code locally. No remote QR service receives the key.
// swift scripts/pairing-qr.swift /private/path/phone.code /private/path/phone.png
import Foundation
import CoreImage
import AppKit
import Darwin

func fail(_ message: String) -> Never {
    fputs(message + "\n", stderr)
    exit(1)
}
guard CommandLine.arguments.count == 3 else { fail("Usage: swift scripts/pairing-qr.swift CODE_FILE OUTPUT.png") }
let input = URL(fileURLWithPath: CommandLine.arguments[1])
let output = URL(fileURLWithPath: CommandLine.arguments[2])
do {
    let code = try String(contentsOf: input, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
    guard code.hasPrefix("noches-connect:"), code.utf8.count <= 8192 else { fail("Not a Noches pairing code.") }
    guard let filter = CIFilter(name: "CIQRCodeGenerator") else { fail("QR generator unavailable.") }
    filter.setValue(Data(code.utf8), forKey: "inputMessage")
    filter.setValue("M", forKey: "inputCorrectionLevel")
    guard let qr = filter.outputImage else { fail("The connection code is too large for a QR code.") }
    let padded = qr.composited(over: CIImage(color: .white).cropped(to: qr.extent.insetBy(dx: -4, dy: -4)))
    let scaled = padded.transformed(by: CGAffineTransform(scaleX: 8, y: 8))
    guard let image = CIContext().createCGImage(scaled, from: scaled.extent),
          let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]) else { fail("Could not render QR code.") }
    // Restrictive umask also protects the atomic temporary file.
    let previous = umask(0o077)
    defer { umask(previous) }
    try png.write(to: output, options: .atomic)
    try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: output.path)
    print("Private pairing QR saved. Open it on your computer and scan it with Noches on your phone.")
} catch { fail("Could not read the code or write the image: \(error.localizedDescription)") }
