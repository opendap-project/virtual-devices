// Keeps the virtual device's window above other windows, for QEMU's cocoa display, which has no
// option for it (QEMU 11.1). odap-vdev builds this library with the system's clang and loads it
// into QEMU only (DYLD_INSERT_LIBRARIES) when the window is asked to stay on top (the default).
// It only sets the level of QEMU's own windows; it changes nothing else in QEMU.
#import <AppKit/AppKit.h>

static void keep_on_top(NSWindow *window) {
    window.level = NSFloatingWindowLevel;
    window.collectionBehavior |= NSWindowCollectionBehaviorCanJoinAllSpaces;
}

__attribute__((constructor)) static void odap_window_on_top(void) {
    NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
    [center addObserverForName:NSApplicationDidFinishLaunchingNotification object:nil queue:nil
                    usingBlock:^(NSNotification *note) {
        for (NSWindow *window in NSApp.windows) keep_on_top(window);
    }];
    // Windows QEMU opens or reopens later (a resize, a zoom) keep the level too.
    [center addObserverForName:NSWindowDidBecomeKeyNotification object:nil queue:nil
                    usingBlock:^(NSNotification *note) { keep_on_top(note.object); }];
}
