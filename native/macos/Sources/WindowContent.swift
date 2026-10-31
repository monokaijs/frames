import AppKit

enum WindowContent {
    // nil means AX could not verify the current chrome, not that it disappeared.
    // Keep point insets so a temporary AX failure also survives moves/resizes.
    struct Crop {
        private var insets:(left:CGFloat,top:CGFloat,right:CGFloat,bottom:CGFloat)?

        @MainActor mutating func bounds(frame:CGRect,titleButtons:[CGRect]?)->CGRect {
            if let titleButtons {
                let content=WindowContent.bounds(frame:frame,titleButtons:titleButtons)
                insets=content == frame ? nil : (content.minX-frame.minX,content.minY-frame.minY,frame.maxX-content.maxX,frame.maxY-content.maxY)
            }
            guard let insets,frame.width>insets.left+insets.right,frame.height>insets.top+insets.bottom else{return frame}
            return CGRect(x:frame.minX+insets.left,y:frame.minY+insets.top,
                          width:frame.width-insets.left-insets.right,height:frame.height-insets.top-insets.bottom)
        }
    }

    static func matchingWindow(frame:CGRect,title:String,candidates:[(frame:CGRect,title:String)])->Int? {
        let matches=candidates.indices.filter { index in
            let other=candidates[index].frame
            return abs(frame.minX-other.minX)+abs(frame.minY-other.minY)+abs(frame.width-other.width)+abs(frame.height-other.height)<8
        }
        let titled=matches.filter{candidates[$0].title==title}
        if titled.count==1{return titled[0]}
        return matches.count==1 ? matches[0]:nil
    }
    // macOS can publish a separate, tiny AXDialog over an application's traffic
    // lights. It has no app content and must not become another remote window.
    @MainActor static func isTitlebarAccessory(frame:CGRect,ownerFrames:[CGRect])->Bool {
        ownerFrames.contains { parent in
            guard parent.width>frame.width*2,parent.height>frame.height*2 else{return false}
            let local=NSRect(origin:.zero,size:parent.size)
            let content=NSWindow.contentRect(forFrameRect:local,styleMask:[.titled,.closable,.miniaturizable,.resizable])
            let top=local.height-content.height
            guard top>0,frame.height<=top,frame.width<=top*3 else{return false}
            let header=CGRect(x:parent.minX,y:parent.minY,width:parent.width,height:top)
            return parent.contains(frame) && header.contains(CGPoint(x:frame.midX,y:frame.midY))
        }
    }
    // Use AppKit's current standard title-bar geometry, verified against the
    // host's AX title buttons. Custom/unverifiable chrome remains intact rather
    // than guessing an inset and removing application controls.
    @MainActor static func bounds(frame:CGRect,titleButtons:[CGRect])->CGRect {
        let local=NSRect(origin:.zero,size:frame.size)
        let content=NSWindow.contentRect(forFrameRect:local,styleMask:[.titled,.closable,.miniaturizable,.resizable])
        let top=local.maxY-content.maxY
        let header=CGRect(x:frame.minX,y:frame.minY,width:frame.width,height:top)
        guard top>0,top<frame.height,!titleButtons.isEmpty,
              titleButtons.allSatisfy({ !$0.isEmpty && header.insetBy(dx:-1,dy:-1).contains($0) }) else{return frame}
        return CGRect(x:frame.minX+content.minX,y:frame.minY+top,width:content.width,height:content.height)
    }

    static func pixelRegion(frame:CGRect,content:CGRect,scale:CGFloat)->CGRect {
        CGRect(x:((content.minX-frame.minX)*scale).rounded(),
               y:((content.minY-frame.minY)*scale).rounded(),
               width:(content.width*scale).rounded(),height:(content.height*scale).rounded())
    }
}
