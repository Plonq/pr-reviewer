// Minimal host for exercising the real production bridge without opening a
// user's Gander database or replacing their installed app.
#import <AppKit/AppKit.h>

extern void gander_updater_start(void);
extern int gander_updater_state(void);
extern const char *gander_updater_error(void);
extern void gander_updater_install(void);
extern void gander_updater_retry(void);

@interface SmokeHost : NSObject <NSApplicationDelegate>
@end
@implementation SmokeHost
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    NSBundle *bundle = NSBundle.mainBundle;
    NSString *marker = [bundle objectForInfoDictionaryKey:@"SmokeMarker"];
    if ([[bundle objectForInfoDictionaryKey:@"CFBundleVersion"] isEqualToString:@"2.1"]) {
        [@"relaunched new version" writeToFile:marker atomically:YES encoding:NSUTF8StringEncoding error:NULL];
        [NSApp terminate:nil];
        return;
    }
    gander_updater_start();
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        gander_updater_retry();
    });
    [NSTimer scheduledTimerWithTimeInterval:0.2 repeats:YES block:^(NSTimer *timer) {
        int state = gander_updater_state();
        if (state == 1) {
            if ([[bundle objectForInfoDictionaryKey:@"SmokeQuitOnReady"] boolValue]) {
                NSLog(@"Smoke: staged update ready; quitting normally");
                [NSApp terminate:nil];
                return;
            }
            NSLog(@"Smoke: background download ready; requesting restart");
            gander_updater_install();
        } else if (state == 3) {
            NSLog(@"Smoke failed: %s", gander_updater_error());
            [@"failed" writeToFile:marker atomically:YES encoding:NSUTF8StringEncoding error:NULL];
            [NSApp terminate:nil];
        }
    }];
}
@end

int main(void) {
    @autoreleasepool {
        NSApplication *app = NSApplication.sharedApplication;
        SmokeHost *host = [SmokeHost new];
        app.delegate = host;
        [app run];
    }
    return 0;
}
