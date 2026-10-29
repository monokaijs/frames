import Foundation

// Side-specific device flags from IOKit/hidsystem/IOLLEvent.h. Testing only
// NSEvent's aggregate .control/.command flags loses releases when both sides
// of the same modifier are held.
struct KeyboardModifiers {
    private(set) var held: Set<UInt16> = []
    private(set) var forwarded: Set<UInt16> = []
    private static let groups: [(UInt16, UInt16, UInt, UInt, UInt)] = [
        (0xe0, 0xe4, 1 << 18, 0x0001, 0x2000),
        (0xe1, 0xe5, 1 << 17, 0x0002, 0x0004),
        (0xe2, 0xe6, 1 << 19, 0x0020, 0x0040),
        (0xe3, 0xe7, 1 << 20, 0x0008, 0x0010),
    ]

    mutating func changed(_ usage: UInt16, flags: UInt) {
        guard let (left, right, aggregate, leftMask, rightMask) = Self.groups.first(where: { $0.0 == usage || $0.1 == usage }) else { return }
        if flags & aggregate == 0 {
            held.remove(left); held.remove(right)
        } else if flags & (leftMask | rightMask) != 0 {
            if flags & leftMask != 0 { held.insert(left) } else { held.remove(left) }
            if flags & rightMask != 0 { held.insert(right) } else { held.remove(right) }
        } else {
            // Synthetic events can omit device flags. flagsChanged identifies
            // the changed side; toggle that side instead of releasing its peer.
            if !held.insert(usage).inserted { held.remove(usage) }
        }
    }

    mutating func synchronize(physical: Bool, optionShortcut: Bool = false, suspend: Bool = false) -> [(UInt16, Bool)] {
        var wanted = suspend ? [] : held
        // Option dead keys/IME compose locally. Send Alt only for physical-key
        // input or a physical shortcut (for example Option+Left or Option+F4).
        if !physical && !optionShortcut { wanted.remove(0xe2); wanted.remove(0xe6) }
        let released = forwarded.subtracting(wanted).sorted().map { ($0, false) }
        let pressed = wanted.subtracting(forwarded).sorted().map { ($0, true) }
        forwarded = wanted
        return released + pressed
    }

    mutating func reset() { held.removeAll(); forwarded.removeAll() }
}
