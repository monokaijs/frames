import AppKit

enum WindowContent {
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
