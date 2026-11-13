// All entry points and Sparkle callbacks run on the main thread. The driver is
// retained for the process lifetime; reply blocks are copied and invoked once.
#import <AppKit/AppKit.h>
#import <Sparkle/Sparkle.h>

@interface GanderUpdater : NSObject <SPUUserDriver, SPUUpdaterDelegate>
@property(nonatomic, strong) SPUUpdater *updater;
@property(nonatomic, copy) void (^install)(void);
@property(nonatomic, copy) NSString *error;
@property(nonatomic) BOOL installing;
@end

@implementation GanderUpdater
- (void)fail:(NSError *)error {
    NSLog(@"Gander update: %@", error);
    self.install = nil;
    self.installing = NO;
    self.error = error.localizedDescription;
}
- (BOOL)updater:(SPUUpdater *)updater willInstallUpdateOnQuit:(SUAppcastItem *)item immediateInstallationBlock:(void (^)(void))handler {
    self.install = handler;
    self.error = nil;
    return YES;
}
- (void)showUpdateFoundWithAppcastItem:(SUAppcastItem *)item state:(SPUUserUpdateState *)state reply:(void (^)(SPUUserUpdateChoice))reply {
    if (item.informationOnlyUpdate) {
        self.error = @"This update requires a manual download from GitHub Releases.";
        reply(SPUUserUpdateChoiceDismiss);
        return;
    }
    // Automatic downloads normally reach willInstallUpdateOnQuit. If Sparkle
    // needs permission to install, defer that step to the user's button click.
    self.install = ^{ reply(SPUUserUpdateChoiceInstall); };
}
- (void)showReadyToInstallAndRelaunch:(void (^)(SPUUserUpdateChoice))reply {
    if (self.installing) {
        reply(SPUUserUpdateChoiceInstall);
    } else {
        self.install = ^{ reply(SPUUserUpdateChoiceInstall); };
    }
}
- (void)showUpdatePermissionRequest:(SPUUpdatePermissionRequest *)request reply:(void (^)(SUUpdatePermissionResponse *))reply {
    reply([[SUUpdatePermissionResponse alloc] initWithAutomaticUpdateChecks:YES sendSystemProfile:NO]);
}
- (void)showUserInitiatedUpdateCheckWithCancellation:(void (^)(void))cancellation {}
- (void)showUpdateReleaseNotesWithDownloadData:(SPUDownloadData *)data {}
- (void)showUpdateReleaseNotesFailedToDownloadWithError:(NSError *)error {}
- (void)showUpdateNotFoundWithError:(NSError *)error acknowledgement:(void (^)(void))ack { ack(); }
- (void)showUpdaterError:(NSError *)error acknowledgement:(void (^)(void))ack { [self fail:error]; ack(); }
- (void)showDownloadInitiatedWithCancellation:(void (^)(void))cancellation {}
- (void)showDownloadDidReceiveExpectedContentLength:(uint64_t)length {}
- (void)showDownloadDidReceiveDataOfLength:(uint64_t)length {}
- (void)showDownloadDidStartExtractingUpdate {}
- (void)showExtractionReceivedProgress:(double)progress {}
- (void)showInstallingUpdateWithApplicationTerminated:(BOOL)terminated retryTerminatingApplication:(void (^)(void))retry {}
- (void)showUpdateInstalledAndRelaunched:(BOOL)relaunched acknowledgement:(void (^)(void))ack { ack(); }
- (void)dismissUpdateInstallation { self.install = nil; self.installing = NO; }
- (void)updater:(SPUUpdater *)updater didAbortWithError:(NSError *)error {
    if (![error.domain isEqualToString:SUSparkleErrorDomain] || error.code != SUNoUpdateError) [self fail:error];
}
- (void)updater:(SPUUpdater *)updater didFinishUpdateCycleForUpdateCheck:(SPUUpdateCheck)check error:(NSError *)error {
    if (!error || ([error.domain isEqualToString:SUSparkleErrorDomain] && error.code == SUNoUpdateError)) self.error = nil;
}
- (BOOL)updater:(SPUUpdater *)updater shouldDownloadReleaseNotesForUpdate:(SUAppcastItem *)item { return NO; }
@end

static GanderUpdater *driver;

void gander_updater_start(void) {
    NSCAssert([NSThread isMainThread], @"updater must run on main thread");
    if (driver || ![[NSBundle mainBundle] objectForInfoDictionaryKey:@"SUPublicEDKey"]) return;
    driver = [GanderUpdater new];
    driver.updater = [[SPUUpdater alloc] initWithHostBundle:[NSBundle mainBundle] applicationBundle:[NSBundle mainBundle] userDriver:driver delegate:driver];
    NSError *error = nil;
    if (![driver.updater startUpdater:&error]) [driver fail:error];
}

// 0 idle/downloading, 1 ready, 2 installing, 3 error. Errors are non-modal.
int gander_updater_state(void) {
    if (driver.installing) return 2;
    if (driver.install) return 1;
    return driver.error ? 3 : 0;
}
const char *gander_updater_error(void) { return driver.error.UTF8String; }
void gander_updater_install(void) {
    void (^install)(void) = driver.install;
    if (!install) return;
    driver.install = nil;
    driver.error = nil;
    driver.installing = YES;
    install();
}
void gander_updater_retry(void) {
    if (driver.updater.canCheckForUpdates) {
        driver.error = nil;
        [driver.updater checkForUpdatesInBackground];
    }
}
