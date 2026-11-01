import AppKit

// Shared geometry and policy for independently captured application surfaces.
// Host coordinates have a top-left origin; AppKit client coordinates do not.
enum WindowSurfaces {
    static func isPopup(_ role:String)->Bool { ["menu","tooltip","popover"].contains(role) }

    static func shouldRaise(frontmost:Bool,role:String,hasMenu:Bool,modal:Bool)->Bool {
        !(frontmost && (isPopup(role) || hasMenu || modal))
    }

    static func role(layer:Int,axRole:String?,subrole:String?,modal:Bool)->String {
        if axRole == "AXMenu" { return "menu" }
        if axRole == "AXHelpTag" || subrole == "AXTooltip" { return "tooltip" }
        if axRole == "AXSheet" || axRole == "AXDialog" || subrole == "AXDialog" || subrole == "AXSystemDialog" || modal { return "dialog" }
        if axRole == "AXPopover" || subrole == "AXPopover" {return "popover"}
        // AppKit's menus and help tags are above the ordinary floating levels.
        if layer == Int(CGWindowLevelForKey(.popUpMenuWindow)) { return "menu" }
        if layer == Int(CGWindowLevelForKey(.helpWindow)) { return "tooltip" }
        return layer > 0 ? "popover":"window"
    }

    static func admitted(layer:Int,title:String,alpha:Double,hasAX:Bool,role:String,parent:UInt32?)->Bool {
        guard alpha>0,layer>=0,
              layer<=30 || layer==Int(CGWindowLevelForKey(.popUpMenuWindow)) || layer==Int(CGWindowLevelForKey(.helpWindow)) else{return false}
        if isPopup(role) && parent==nil{return false}
        if layer==0 && title.isEmpty && !hasAX{return false}
        if layer>0 && !hasAX && !["menu","tooltip"].contains(role){return false}
        return true
    }

    static func bounds(_ remote:[String:Any])->CGRect {
        let b=remote["bounds"] as? [String:Double] ?? [:]
        return CGRect(x:b["x"] ?? 0,y:b["y"] ?? 0,width:b["width"] ?? 1,height:b["height"] ?? 1)
    }

    static func parent(owners:Set<UInt32>,preferred:[UInt32?])->UInt32? {
        for id in preferred {if let id,owners.contains(id){return id}}
        return owners.count==1 ? owners.first:nil
    }

    static func acyclicParents(_ parents:[UInt32:UInt32],live:Set<UInt32>)->[UInt32:UInt32] {
        parents.filter { child,parent in
            guard live.contains(child),live.contains(parent) else{return false}
            var current=parent,seen:Set<UInt32>=[child]
            while seen.insert(current).inserted {
                guard let next=parents[current] else{return true};current=next
            }
            return false
        }
    }

    static func attachedRect(child:CGRect,parent:CGRect,displayed:CGRect)->CGRect {
        let sx=displayed.width/max(1,parent.width),sy=displayed.height/max(1,parent.height)
        return CGRect(x:displayed.minX+(child.minX-parent.minX)*sx,
                      y:displayed.maxY-(child.maxY-parent.minY)*sy,
                      width:child.width*sx,height:child.height*sy)
    }

    // Children may precede their owners on the wire, including nested submenus.
    static func ordered(_ windows:[[String:Any]])->[[String:Any]] {
        func key(_ value:[String:Any])->String { "\(value["id"] ?? 0):\(value["generation"] ?? 0)" }
        let byID=Dictionary(windows.compactMap { w -> (String,[String:Any])? in
            guard let id=w["key"] as? [String:Any] else{return nil};return (key(id),w)
        },uniquingKeysWith:{first,_ in first})
        func depth(_ w:[String:Any])->Int {
            var current=w,seen=Set<String>(),depth=0
            while let p=current["parent"] as? [String:Any],let parent=byID[key(p)],seen.insert(key(p)).inserted,depth<32 {
                depth+=1;current=parent
            }
            return depth
        }
        return windows.enumerated().sorted {
            let a=depth($0.element),b=depth($1.element)
            return a == b ? $0.offset<$1.offset:a<b
        }.map(\.element)
    }
}
