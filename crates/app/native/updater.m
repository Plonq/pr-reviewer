// All entry points and Sparkle callbacks run on the main thread. The driver is
// retained for the process lifetime; reply blocks are copied and invoked once.
#import <AppKit/AppKit.h>
#import <Sparkle/Sparkle.h>

@interface GanderUpdater : NSObject <SPUUserDriver, SPUUpdaterDelegate>
@property(nonatomic, strong) SPUUpdater *updater;
@property(nonatomic, copy) void (^install)(void);
@property(nonatomic, copy) NSString *error;
@property(nonatomic) BOOL installing;
@property(nonatomic) BOOL started;
@property(nonatomic) BOOL manualCheck;
@property(nonatomic) BOOL downloading;
@property(nonatomic) NSTimeInterval upToDateUntil;
@end

@implementation GanderUpdater
- (void)fail:(NSError *)error {
    NSLog(@"Gander update: %@", error);
    self.install = nil;
    self.installing = NO;
    self.manualCheck = NO;
    self.downloading = NO;
    self.upToDateUntil = 0;
    self.error = error.localizedDescription;
}
- (BOOL)updater:(SPUUpdater *)updater willInstallUpdateOnQuit:(SUAppcastItem *)item immediateInstallationBlock:(void (^)(void))handler {
    self.install = handler;
    self.error = nil;
    self.manualCheck = NO;
    return YES;
}
- (void)showUpdateFoundWithAppcastItem:(SUAppcastItem *)item state:(SPUUserUpdateState *)state reply:(void (^)(SPUUserUpdateChoice))reply {
    if (item.informationOnlyUpdate) {
        self.error = @"This update requires a manual download from GitHub Releases.";
        reply(SPUUserUpdateChoiceDismiss);
        return;
    }
    if (state.stage == SPUUserUpdateStageNotDownloaded) {
        // A manual check uses Sparkle's user-initiated API. Download quietly
        // rather than showing an alert; never relaunch until the button click.
        self.downloading = YES;
        reply(SPUUserUpdateChoiceInstall);
        return;
    }
    // Automatic downloads normally reach willInstallUpdateOnQuit. If Sparkle
    // needs permission to install, defer that step to the user's button click.
    self.install = ^{ reply(SPUUserUpdateChoiceInstall); };
    self.manualCheck = NO;
}
- (void)showReadyToInstallAndRelaunch:(void (^)(SPUUserUpdateChoice))reply {
    if (self.installing) {
        reply(SPUUserUpdateChoiceInstall);
    } else {
        self.install = ^{ reply(SPUUserUpdateChoiceInstall); };
        self.manualCheck = NO;
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
- (void)showDownloadInitiatedWithCancellation:(void (^)(void))cancellation { self.downloading = YES; }
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
    BOOL noUpdate = [error.domain isEqualToString:SUSparkleErrorDomain] && error.code == SUNoUpdateError;
    if (self.manualCheck && noUpdate) self.upToDateUntil = [NSDate timeIntervalSinceReferenceDate] + 5;
    if (!error || noUpdate) self.error = nil;
    self.manualCheck = NO;
    self.downloading = NO;
}
- (void)updater:(SPUUpdater *)updater willDownloadUpdate:(SUAppcastItem *)item withRequest:(NSMutableURLRequest *)request { self.downloading = YES; }
- (BOOL)updater:(SPUUpdater *)updater shouldDownloadReleaseNotesForUpdate:(SUAppcastItem *)item { return NO; }
@end

static GanderUpdater *driver;

void gander_updater_start(void) {
    NSCAssert([NSThread isMainThread], @"updater must run on main thread");
    if (driver || ![[NSBundle mainBundle] objectForInfoDictionaryKey:@"SUPublicEDKey"]) return;
    driver = [GanderUpdater new];
    driver.updater = [[SPUUpdater alloc] initWithHostBundle:[NSBundle mainBundle] applicationBundle:[NSBundle mainBundle] userDriver:driver delegate:driver];
    NSError *error = nil;
    driver.started = [driver.updater startUpdater:&error];
    if (!driver.started) {
        [driver fail:error];
    } else if (driver.updater.automaticallyChecksForUpdates) {
        // Sparkle explicitly supports forcing a background check immediately
        // after startup. Don't wait for the persisted hourly schedule.
        [driver.updater checkForUpdatesInBackground];
    }
}

// 0 quiet idle/download, 1 ready, 2 installing, 3 error, 4 manual check,
// 5 briefly up to date, 6 manual download/preparation. No modal status UI.
int gander_updater_state(void) {
    if (driver.installing) return 2;
    if (driver.install) return 1;
    if (driver.error) return 3;
    if (driver.manualCheck) return driver.downloading ? 6 : 4;
    if (driver.upToDateUntil > [NSDate timeIntervalSinceReferenceDate]) return 5;
    return 0;
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
void gander_updater_check(void) {
    NSCAssert([NSThread isMainThread], @"updater must run on main thread");
    if (!driver.started || driver.install || driver.installing || driver.manualCheck) return;
    // Join a background check/download already in progress, without starting a
    // second cycle or accidentally installing a staged update.
    if (!driver.updater.sessionInProgress && !driver.updater.canCheckForUpdates) return;
    driver.error = nil;
    driver.upToDateUntil = 0;
    driver.manualCheck = YES;
    if (!driver.updater.sessionInProgress) [driver.updater checkForUpdates];
}
