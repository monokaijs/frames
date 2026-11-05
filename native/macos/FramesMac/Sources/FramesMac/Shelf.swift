import AppKit
import UniformTypeIdentifiers

// The received files shelf (ABI 5), as v1 had it: files fetched from the
// host this session, each a row to drag into another application. It opens
// when a fetch starts or a file arrives; closing it only hides it. Built
// from the session's state (`transfers`, `received`).

@MainActor
final class FileShelf: NSObject, NSWindowDelegate {
    static let shared = FileShelf()

    private let window: NSWindow
    private let rows = NSStackView()
    private var shown: [String] = []
    private var fetching: [String] = []

    override init() {
        window = NSWindow(
            contentRect: NSRect(x: 180, y: 180, width: 550, height: 360), styleMask: [.titled, .closable, .resizable],
            backing: .buffered, defer: false)
        super.init()
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.title = FileShelf.idleTitle
        let content = NSView(frame: NSRect(x: 0, y: 0, width: 550, height: 360))
        let cancel = NSButton(title: "Cancel transfers", target: self, action: #selector(cancelTransfers))
        cancel.frame = NSRect(x: 12, y: 8, width: 160, height: 28)
        content.addSubview(cancel)
        rows.orientation = .vertical
        rows.alignment = .leading
        rows.spacing = 4
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 44, width: 550, height: 316))
        scroll.autoresizingMask = [.width, .height]
        scroll.hasVerticalScroller = true
        let document = FlippedView(frame: NSRect(x: 0, y: 0, width: 530, height: 316))
        rows.frame = document.bounds
        rows.autoresizingMask = [.width]
        document.addSubview(rows)
        scroll.documentView = document
        content.addSubview(scroll)
        window.contentView = content
    }

    static let idleTitle = "Frames received files — drag a file to another app"

    /// The session's state changed.
    func update(transfers: [String], received: [String]) {
        var opened = false
        for path in received where !shown.contains(path) {
            shown.append(path)
            rows.addArrangedSubview(PromisedFileRow(url: URL(fileURLWithPath: path)))
            opened = true
        }
        if let documentView = rows.superview {
            documentView.frame.size.height = max(316, CGFloat(shown.count) * 40 + 8)
            rows.frame = documentView.bounds
        }
        if transfers != fetching {
            opened = opened || transfers.count > fetching.count
            fetching = transfers
        }
        if let name = fetching.first {
            let file = (name as NSString).lastPathComponent
            window.title = "Frames transfers — \(file) (fetching\(fetching.count > 1 ? ", \(fetching.count) files" : "")…)"
        } else {
            window.title = FileShelf.idleTitle
        }
        if opened { window.orderFront(nil) }
    }

    @objc private func cancelTransfers() {
        sendViewerCommand(["command": "cancel_transfers"])
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        window.orderOut(nil)
        return false
    }
}

/// Top-down rows.
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

/// A received file: its icon and name, dragged out as a file promise.
@MainActor
final class PromisedFileRow: NSView, NSFilePromiseProviderDelegate, NSDraggingSource {
    nonisolated let url: URL

    init(url: URL) {
        self.url = url
        super.init(frame: NSRect(x: 0, y: 0, width: 520, height: 36))
        let label = NSTextField(labelWithString: url.lastPathComponent)
        label.frame = NSRect(x: 40, y: 6, width: 470, height: 24)
        label.lineBreakMode = .byTruncatingMiddle
        addSubview(label)
        toolTip = url.path
        translatesAutoresizingMaskIntoConstraints = false
        widthAnchor.constraint(equalToConstant: 520).isActive = true
        heightAnchor.constraint(equalToConstant: 36).isActive = true
    }

    required init?(coder: NSCoder) { nil }

    override func draw(_ dirtyRect: NSRect) {
        NSWorkspace.shared.icon(forFile: url.path).draw(in: NSRect(x: 0, y: 2, width: 32, height: 32))
    }

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 { NSWorkspace.shared.activateFileViewerSelecting([url]) }
    }

    override func mouseDragged(with event: NSEvent) {
        let type = UTType(filenameExtension: url.pathExtension) ?? .data
        let provider = NSFilePromiseProvider(fileType: type.identifier, delegate: self)
        let item = NSDraggingItem(pasteboardWriter: provider)
        item.setDraggingFrame(NSRect(x: 0, y: 0, width: 32, height: 32), contents: NSWorkspace.shared.icon(forFile: url.path))
        beginDraggingSession(with: [item], event: event, source: self)
    }

    nonisolated func filePromiseProvider(_ filePromiseProvider: NSFilePromiseProvider, fileNameForType fileType: String)
        -> String
    {
        url.lastPathComponent
    }

    nonisolated func filePromiseProvider(
        _ filePromiseProvider: NSFilePromiseProvider, writePromiseTo target: URL,
        completionHandler: @escaping (Error?) -> Void
    ) {
        let source = url
        do {
            try FileManager.default.copyItem(at: source, to: target)
            completionHandler(nil)
        } catch {
            completionHandler(error)
        }
    }

    func draggingSession(_ session: NSDraggingSession, sourceOperationMaskFor context: NSDraggingContext)
        -> NSDragOperation
    {
        .copy
    }
}
