import AppKit
import ApplicationServices

// AXWindows omits sheets and menu windows in many AppKit applications. Keep
// explicit sheet ownership, and use AX hit testing for independently drawn menus.
@MainActor final class AXSurfaces {
    struct Entry { let element:AXUIElement;let parent:AXUIElement? }
    let application:AXUIElement
    var entries:[Entry]=[]
    private var inspected=0
    private let deadline=ProcessInfo.processInfo.systemUptime+0.15

    init(pid:pid_t) {
        application=AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(application,0.1)
        guard AXIsProcessTrusted() else{return}
        var roots=Self.attribute(application,kAXWindowsAttribute) as? [AXUIElement] ?? []
        for name in [kAXFocusedWindowAttribute,kAXMainWindowAttribute] {
            if let element=Self.element(application,name),!roots.contains(where:{CFEqual($0,element)}) {roots.append(element)}
        }
        for root in roots.prefix(128) { append(root,parent:nil,depth:0) }
    }

    static func attribute(_ element:AXUIElement,_ name:String)->CFTypeRef? {
        var value:CFTypeRef?
        return AXUIElementCopyAttributeValue(element,name as CFString,&value) == .success ? value:nil
    }
    static func element(_ element:AXUIElement,_ name:String)->AXUIElement? {
        guard let value=attribute(element,name),CFGetTypeID(value)==AXUIElementGetTypeID() else{return nil}
        return (value as! AXUIElement)
    }
    static func frame(_ element:AXUIElement)->CGRect? {
        guard let p=attribute(element,kAXPositionAttribute),let s=attribute(element,kAXSizeAttribute),
              CFGetTypeID(p)==AXValueGetTypeID(),CFGetTypeID(s)==AXValueGetTypeID() else{return nil}
        var point=CGPoint.zero,size=CGSize.zero
        guard AXValueGetValue(p as! AXValue,.cgPoint,&point),AXValueGetValue(s as! AXValue,.cgSize,&size),size.width>0,size.height>0 else{return nil}
        return CGRect(origin:point,size:size)
    }
    private func append(_ element:AXUIElement,parent:AXUIElement?,depth:Int) {
        guard entries.count<128,depth<8,ProcessInfo.processInfo.systemUptime<deadline,!entries.contains(where:{CFEqual($0.element,element)}) else{return}
        AXUIElementSetMessagingTimeout(element,0.1)
        entries.append(Entry(element:element,parent:parent))
        let sheets=Self.attribute(element,"AXSheets") as? [AXUIElement] ?? []
        let children=Self.attribute(element,kAXChildrenAttribute) as? [AXUIElement] ?? []
        for child in sheets+children.prefix(128) {
            guard inspected<256,ProcessInfo.processInfo.systemUptime<deadline else{break};inspected+=1
            AXUIElementSetMessagingTimeout(child,0.05)
            let role=Self.attribute(child,kAXRoleAttribute) as? String ?? ""
            let subrole=Self.attribute(child,kAXSubroleAttribute) as? String ?? ""
            if ["AXSheet","AXWindow","AXDialog","AXMenu","AXPopover"].contains(role) || ["AXDialog","AXSystemDialog","AXPopover"].contains(subrole) {
                append(child,parent:element,depth:depth+1)
            }
        }
    }
    func hitTest(frame:CGRect)->AXUIElement? {
        guard AXIsProcessTrusted() else{return nil}
        var hit:AXUIElement?
        guard AXUIElementCopyElementAtPosition(application,Float(frame.midX),Float(frame.midY),&hit) == .success else{return nil}
        var current=hit
        for _ in 0..<16 {
            guard let element=current else{break}
            if let rect=Self.frame(element),WindowContent.matchingWindow(frame:frame,title:"",candidates:[(rect,"")]) != nil {
                let role=Self.attribute(element,kAXRoleAttribute) as? String ?? ""
                if ["AXMenu","AXSheet","AXWindow","AXHelpTag","AXPopover"].contains(role) {return element}
            }
            current=Self.element(element,kAXParentAttribute)
        }
        return nil
    }
}
