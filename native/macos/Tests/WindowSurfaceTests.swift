import AppKit

@main struct WindowSurfaceTests {
    @MainActor static func main() {
        if CommandLine.arguments.contains("--dock") {return}
        _ = NSApplication.shared
        let popupLevel=Int(CGWindowLevelForKey(.popUpMenuWindow)),helpLevel=Int(CGWindowLevelForKey(.helpWindow))
        precondition(popupLevel>30)
        precondition(WindowSurfaces.shouldRaise(frontmost:true,role:"window",hasMenu:false,modal:false))
        precondition(!WindowSurfaces.shouldRaise(frontmost:true,role:"window",hasMenu:true,modal:false))
        precondition(!WindowSurfaces.shouldRaise(frontmost:true,role:"menu",hasMenu:false,modal:false))
        precondition(!WindowSurfaces.shouldRaise(frontmost:true,role:"dialog",hasMenu:false,modal:true))
        precondition(WindowSurfaces.shouldRaise(frontmost:false,role:"menu",hasMenu:true,modal:false))
        precondition(WindowSurfaces.role(layer:popupLevel,axRole:nil,subrole:nil,modal:false)=="menu")
        precondition(WindowSurfaces.role(layer:helpLevel,axRole:nil,subrole:nil,modal:false)=="tooltip")
        precondition(WindowSurfaces.role(layer:0,axRole:"AXSheet",subrole:nil,modal:false)=="dialog")
        precondition(WindowSurfaces.role(layer:0,axRole:"AXDialog",subrole:nil,modal:false)=="dialog")
        precondition(WindowSurfaces.role(layer:0,axRole:"AXPopover",subrole:nil,modal:false)=="popover")
        precondition(WindowSurfaces.role(layer:0,axRole:"AXWindow",subrole:"AXDialog",modal:false)=="dialog")
        precondition(WindowSurfaces.role(layer:3,axRole:"AXWindow",subrole:nil,modal:false)=="popover")
        precondition(WindowSurfaces.admitted(layer:0,title:"",alpha:1,hasAX:true,role:"dialog",parent:1))
        precondition(WindowSurfaces.admitted(layer:popupLevel,title:"",alpha:1,hasAX:false,role:"menu",parent:1))
        precondition(!WindowSurfaces.admitted(layer:popupLevel,title:"",alpha:1,hasAX:false,role:"menu",parent:nil))
        precondition(!WindowSurfaces.admitted(layer:0,title:"",alpha:1,hasAX:false,role:"window",parent:nil))
        precondition(!WindowSurfaces.admitted(layer:3,title:"Helper",alpha:1,hasAX:false,role:"popover",parent:1))
        precondition(!WindowSurfaces.admitted(layer:0,title:"Transparent",alpha:0,hasAX:true,role:"window",parent:nil))
        precondition(!WindowSurfaces.admitted(layer:1000,title:"System",alpha:1,hasAX:true,role:"popover",parent:1))
        precondition(WindowSurfaces.parent(owners:[1,2],preferred:[9,2,1])==2)
        precondition(WindowSurfaces.parent(owners:[1,2],preferred:[9,nil])==nil)
        precondition(WindowSurfaces.parent(owners:[1],preferred:[])==1)
        precondition(WindowSurfaces.acyclicParents([2:1,3:2,4:5,5:4,6:99],live:[1,2,3,4,5,6])==[2:1,3:2])

        // Capture the production control messages, including real NSEvent input.
        let output=Pipe(),original=dup(STDOUT_FILENO)
        dup2(output.fileHandleForWriting.fileDescriptor,STDOUT_FILENO)
        func remote(_ id:Int,_ role:String="window",parent:Int?=nil,modal:Bool=false,x:Double=100,y:Double=100,w:Double=800,h:Double=600)->[String:Any] {
            ["key":["id":id,"generation":1],"app_id":"fixture","title":"Fixture \(id)","role":role,
             "parent":parent.map{["id":$0,"generation":1]} as Any? ?? NSNull(),"modal":modal,
             "bounds":["x":x,"y":y,"width":w,"height":h],"scale":2.0,"resize":"fixed","visible":true]
        }
        func key(_ id:Int)->String{"\(id):1"}
        let client=NativeClient(host:"popup-fixture")
        let root=remote(1),menu=remote(2,"menu",parent:1,x:760,y:160,w:200,h:240)
        let submenu=remote(3,"menu",parent:2,x:950,y:180,w:160,h:120)
        // Deliberately reversed input: a submenu must attach to an existing menu.
        client.update(["type":"windows","windows":[submenu,menu,root]])
        let parent=client.windows[key(1)]!,popup=client.windows[key(2)]!,sub=client.windows[key(3)]!
        precondition(popup.window.parent === parent.window && sub.window.parent === popup.window)
        precondition(!popup.window.canBecomeKey && popup.window.styleMask == [.borderless])
        precondition(popup.view.acceptsFirstMouse(for:nil))
        precondition(popup.menuButton==nil && popup.fill && popup.mode=="stretch_local")
        parent.view.picture=CGSize(width:800,height:600)
        parent.window.setContentSize(CGSize(width:400,height:300))
        parent.window.setFrameOrigin(CGPoint(x:200,y:200));client.layoutWindowGroups()
        let displayed=parent.window.convertToScreen(parent.view.convert(parent.view.contentRect(),to:nil))
        let expected=WindowSurfaces.attachedRect(child:WindowSurfaces.bounds(menu),parent:WindowSurfaces.bounds(root),displayed:displayed)
        precondition(popup.window.frame==expected)
        precondition(popup.window.frame.maxX>parent.window.frame.maxX) // No parent clipping.
        let old=popup.window.frame
        parent.window.setFrameOrigin(parent.window.frame.origin.applying(CGAffineTransform(translationX:-120,y:70)))
        client.layoutWindowGroups()
        precondition(popup.window.frame==old.offsetBy(dx:-120,dy:70))

        // Drag events still addressed to the parent must hit the top submenu.
        let screen=CGPoint(x:sub.window.frame.midX,y:sub.window.frame.midY)
        let location=parent.window.convertPoint(fromScreen:screen)
        let drag=NSEvent.mouseEvent(with:.rightMouseDragged,location:location,modifierFlags:[],timestamp:0,windowNumber:parent.window.windowNumber,context:nil,eventNumber:1,clickCount:1,pressure:1)!
        let(target,point)=parent.view.pointerDestination(drag)
        precondition(target === sub)
        precondition(abs((point["x"] as! NSNumber).doubleValue-0.5)<0.001 && abs((point["y"] as! NSNumber).doubleValue-0.5)<0.001)
        parent.view.pointer(drag,kind:"pointer")
        client.dismissRemoteMenus()
        client.dismissRemoteMenus() // A local menu opening after its button click must not send a second Escape.

        // Remote movement/resizing updates existing surfaces, not just creation.
        var movedMenu=menu;movedMenu["bounds"]=["x":200.0,"y":220.0,"width":240.0,"height":180.0]
        client.update(["type":"windows","windows":[root,movedMenu,submenu]])
        precondition(client.windows[key(2)] === popup)
        precondition(popup.window.frame.width==120 && popup.window.frame.height==90)
        parent.setShown(false)
        precondition(!popup.window.isVisible && !sub.window.isVisible)
        parent.setShown(true)
        precondition(popup.window.isVisible && sub.window.isVisible)

        // Letterboxing, nonuniform stretch, Retina, and negative host origins.
        let rect=WindowSurfaces.attachedRect(child:CGRect(x:-700,y:300,width:120,height:80),parent:CGRect(x:-800,y:100,width:800,height:600),displayed:CGRect(x:40,y:60,width:400,height:150))
        precondition(rect==CGRect(x:90,y:140,width:60,height:20))

        let tooltip=remote(4,"tooltip",parent:1,x:180,y:180,w:100,h:20)
        client.update(["type":"windows","windows":[root,tooltip]])
        let tip=client.windows[key(4)]!
        precondition(tip.window.ignoresMouseEvents && !tip.window.canBecomeKey)
        precondition(popup.window.parent==nil && sub.window.parent==nil && !popup.window.isVisible)

        let dialog=remote(5,"dialog",parent:1,modal:true,x:250,y:200,w:360,h:200)
        let independent=remote(6,x:1200)
        client.update(["type":"windows","windows":[dialog,root,independent]])
        let sheet=client.windows[key(5)]!,other=client.windows[key(6)]!
        precondition(sheet.window.parent === parent.window && sheet.window.canBecomeKey)
        let dialogPosition=sheet.window.frame.origin.applying(CGAffineTransform(translationX:25,y:-30))
        sheet.window.setFrameOrigin(dialogPosition);client.layoutWindowGroups()
        precondition(sheet.window.frame.origin==dialogPosition) // Local dialog dragging must not snap back.
        precondition(client.modalBlocker(for:parent) === sheet)
        precondition(client.modalBlocker(for:other)==nil && client.modalBlocker(for:sheet)==nil)
        sheet.setShown(false)
        precondition(client.modalBlocker(for:parent) === sheet) // Hiding a modal does not unblock its parent.
        sheet.setShown(true)
        precondition(client.survivingOwner(of:sheet,keys:[key(1),key(6)]) === parent)
        precondition(client.survivingOwner(of:sheet,keys:[key(6)])==nil)
        parent.send("input",["input":["kind":"button","button":0,"down":true]])
        sheet.send("input",["input":["kind":"key","usage":41,"down":true]])
        sheet.send("input",["input":["kind":"key","usage":41,"down":false]])
        other.send("input",["input":["kind":"key","usage":40,"down":true]])
        client.update(["type":"windows","windows":[root,independent]])
        precondition(client.modalBlocker(for:parent)==nil && sheet.window.parent==nil)
        parent.send("input",["input":["kind":"key","usage":40,"down":true]])
        let appModal=remote(7,"dialog",modal:true,w:320,h:180)
        client.update(["type":"windows","windows":[root,independent,appModal]])
        let alert=client.windows[key(7)]!
        precondition(client.modalBlocker(for:parent) === alert && client.modalBlocker(for:other) === alert)
        var foreign=remote(8);foreign["app_id"]="another-application"
        let foreignProxy=RemoteProxy(remote:foreign,host:"popup-fixture")
        precondition(client.modalBlocker(for:foreignProxy)==nil)
        foreignProxy.destroy()
        let nestedAlert=remote(9,"dialog",parent:7,modal:true,w:200,h:120)
        client.update(["type":"windows","windows":[root,independent,appModal,nestedAlert]])
        let innermost=client.windows[key(9)]!
        precondition(client.modalBlocker(for:parent) === innermost)
        precondition(client.modalBlocker(for:alert) === innermost && client.modalBlocker(for:innermost)==nil)
        let laterAlert=remote(10,"dialog",modal:true,w:220,h:140)
        client.update(["type":"windows","windows":[root,independent,appModal,laterAlert]])
        precondition(client.modalBlocker(for:parent) === client.windows[key(10)]!)
        precondition(client.modalBlocker(for:alert) === client.windows[key(10)]!)
        precondition(client.modalBlocker(for:client.windows[key(10)]!)==nil)

        // Reparent an existing popup, then remove its owner relationship.
        var reparented=menu;reparented["parent"]=["id":6,"generation":1]
        client.update(["type":"windows","windows":[root,independent,reparented]])
        let reused=client.windows[key(2)]!
        precondition(reused.window.parent === other.window)
        reparented["parent"]=NSNull()
        client.update(["type":"windows","windows":[root,independent,reparented]])
        precondition(reused.window.parent==nil)
        client.update(["type":"windows","windows":[]])
        if let monitor=client.outsideClickMonitor{NSEvent.removeMonitor(monitor);client.outsideClickMonitor=nil}

        dup2(original,STDOUT_FILENO);close(original);output.fileHandleForWriting.closeFile()
        let events=String(data:output.fileHandleForReading.readDataToEndOfFile(),encoding:.utf8)!.split(separator:"\n").map{
            try! JSONSerialization.jsonObject(with:Data($0.utf8)) as! [String:Any]
        }
        let input=events.filter{$0["event"] as? String == "input"}
        func windowID(_ event:[String:Any])->Int{(event["window"] as! [String:Int])["id"]!}
        precondition(input.contains{windowID($0)==3 && ($0["input"] as? [String:Any])?["kind"] as? String == "pointer"})
        precondition(!input.contains{windowID($0)==1 && ($0["input"] as? [String:Any])?["kind"] as? String == "button"})
        precondition(input.filter{[2,3].contains(windowID($0)) && ($0["input"] as? [String:Any])?["usage"] as? Int == 41}.count==2)
        precondition(input.contains{windowID($0)==5} && input.contains{windowID($0)==6} && input.contains{windowID($0)==1})
        print("PASS: Mac surface admission/ownership; nested menu geometry, scaling and dragging; modal isolation; dismissal; tooltip hit testing; reparenting and cleanup")
    }
}
