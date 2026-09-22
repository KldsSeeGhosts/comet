import SwiftUI
import VisionKit

/// Only extracts pairing text. The pairing sheet still validates and saves it
/// after the user taps Connect; scanning itself never grants access.
struct CompanionScanner: UIViewControllerRepresentable {
    let scanned: (String) -> Void
    let failed: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(scanned: scanned, failed: failed) }
    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced, recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false, isPinchToZoomEnabled: true,
            isGuidanceEnabled: true, isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        do { try scanner.startScanning() }
        catch { DispatchQueue.main.async { failed("Camera unavailable. Allow camera access in Settings, or paste your connection code.") } }
        return scanner
    }
    func updateUIViewController(_ controller: DataScannerViewController, context: Context) {}
    static func dismantleUIViewController(_ controller: DataScannerViewController, coordinator: Coordinator) {
        controller.stopScanning()
    }
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let scanned: (String) -> Void
        let failed: (String) -> Void
        var finished = false
        init(scanned: @escaping (String) -> Void, failed: @escaping (String) -> Void) {
            self.scanned = scanned; self.failed = failed
        }
        func dataScanner(_ scanner: DataScannerViewController, didAdd addedItems: [RecognizedItem], allItems: [RecognizedItem]) {
            guard !finished else { return }
            for item in addedItems {
                if case .barcode(let barcode) = item, let text = barcode.payloadStringValue, text.hasPrefix("noches-connect:") {
                    finished = true; scanner.stopScanning(); scanned(text); return
                }
            }
        }
        func dataScanner(_ scanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) {
            guard !finished else { return }
            finished = true
            failed("Camera unavailable. Paste your connection code to continue.")
        }
    }
}
