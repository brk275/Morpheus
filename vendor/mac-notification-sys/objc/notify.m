#import "notify.h"
#import "owned.h"
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <limits.h>
#include <string.h>

// Seconds to wait for async XPC delivery confirmation before giving up.
// Matched by DELIVERY_TIMEOUT_SECS in bridge.rs.
static const NSTimeInterval kDeliveryTimeoutSecs = 2.0;

// getBundleIdentifier(app_name: &str) -> "com.apple.Terminal"
NSString* getBundleIdentifier(NSString* appName) {
    NSString* findString = [NSString stringWithFormat:@"get id of application \"%@\"", appName];
    NSAppleScript* findScript = [[NSAppleScript alloc] initWithSource:findString];
    NSAppleEventDescriptor* resultDescriptor = [findScript executeAndReturnError:nil];
    return [resultDescriptor stringValue];
}

// setApplication(new_bundle_identifier: &str) -> Result<()>
// invariant: this function should be called at most once and before `sendNotification`
BOOL setApplication(NSString* newbundleIdentifier) {
    @autoreleasepool {
        if (!installNSBundleHook()) {
            return NO;
        }
        if (LSCopyApplicationURLsForBundleIdentifier((CFStringRef)newbundleIdentifier, NULL) != NULL) {
            [fakeBundleIdentifier release];
            fakeBundleIdentifier = newbundleIdentifier;
            [newbundleIdentifier retain];
            return YES;
        }
        return NO;
    }
}

// handles both file:// and bare paths
NSImage* getImageFromURL(NSString* url) {
    NSURL* imageURL = [NSURL URLWithString:url];
    if ([[imageURL scheme] length] == 0) {
        imageURL = [NSURL fileURLWithPath:url];
    }
    return [[NSImage alloc] initWithContentsOfURL:imageURL];
}

// Extracts the raw UUID bytes from a notification's identifier string.
// NSUserNotification.identifier is an NSString (UUID canonical form); we reconstruct
// the NSUUID and write 16 bytes into `out` so Rust callbacks receive plain bytes.
// If the identifier is nil or not a valid UUID string (e.g. on very old OS versions
// or for notifications we did not create), out is zeroed — the Rust side will find
// no matching PENDING entry and silently discard the callback.
static void uuidBytesFromNotification(NSUserNotification* n, unsigned char out[16]) {
    NSUUID* uuid = n.identifier ? [[NSUUID alloc] initWithUUIDString:n.identifier] : nil;
    if (uuid) {
        [uuid getUUIDBytes:out];
        [uuid release];
    } else {
        memset(out, 0, 16);
    }
}

// resolveAutoDismiss — called on the main thread when wasAutoDismissed() fires.
// NSUserNotification has no auto-dismiss delegate callback; polling deliveredNotifications is the
// only signal. We drain any already-queued delegate messages (didActivate / didDismissAlert) before
// falling back to treating the disappearance as a silent auto-dismiss. This removes all timing
// heuristics: if a real callback was queued, it fires during the runUntilDate: drain and wins.
static void resolveAutoDismiss(const unsigned char* uuid) {
    if (rust_notification_is_done(uuid))
        return; // a real callback already won
    // Drain delegate messages already queued on this run loop, then re-check.
    [[NSRunLoop currentRunLoop] runUntilDate:[NSDate date]];
    if (rust_notification_is_done(uuid))
        return;
    rust_notification_auto_dismissed(uuid);
}

// sendNotification — delivers or schedules a notification identified by notificationId (16 bytes).
// shouldWait: if NO, returns immediately after delivery (fire-and-forget).
// if YES, blocks until the user interacts or the notification is auto-dismissed.
// Result is communicated back to Rust via rust_notification_activated / rust_notification_dismissed /
// rust_notification_auto_dismissed callbacks.
void sendNotification(NSString* title, NSString* subtitle, NSString* message, NSDictionary* options, const unsigned char* notificationId, BOOL shouldWait) {
    @autoreleasepool {
        NSUserNotificationCenter* notificationCenter = [NSUserNotificationCenter defaultUserNotificationCenter];

        NSUserNotification* userNotification = [[NSUserNotification alloc] init];
        BOOL isScheduled = NO;

        NSUUID* uuid = [[NSUUID alloc] initWithUUIDBytes:notificationId];
        NSString* identifierString = [uuid UUIDString];
        userNotification.identifier = identifierString;
        [uuid release];

        // Basic text
        userNotification.title = title;
        if (![subtitle isEqualToString:@""]) {
            userNotification.subtitle = subtitle;
        }
        userNotification.informativeText = message;

        // Notification sound
        if (options[@"sound"] && ![options[@"sound"] isEqualToString:@""]) {
            if ([options[@"sound"] isEqualToString:@"NSUserNotificationDefaultSoundName"]) {
                userNotification.soundName = NSUserNotificationDefaultSoundName;
            } else {
                userNotification.soundName = options[@"sound"];
            }
        }

        // Delivery Date/Schedule
        if (options[@"deliveryDate"] && ![options[@"deliveryDate"] isEqualToString:@""]) {
            double deliveryDate = [options[@"deliveryDate"] doubleValue];
            userNotification.deliveryDate = [NSDate dateWithTimeIntervalSince1970:deliveryDate];
            isScheduled = YES;
        }

        // Main Actions Button (defaults to "Show")
        if (options[@"mainButtonLabel"] && ![options[@"mainButtonLabel"] isEqualToString:@""]) {
            userNotification.actionButtonTitle = options[@"mainButtonLabel"];
            userNotification.hasActionButton = 1;
        } else {
            userNotification.hasActionButton = 0;
        }

        // Dropdown actions
        if (options[@"actions"] && ![options[@"actions"] isEqualToString:@""]) {
            [userNotification setValue:@YES forKey:@"_showsButtons"];
            NSArray* myActions = [options[@"actions"] componentsSeparatedByString:@","];
            if (myActions.count > 1) {
                [userNotification setValue:@YES forKey:@"_alwaysShowAlternateActionMenu"];
                [userNotification setValue:myActions forKey:@"_alternateActionButtonTitles"];
            }
        }

        // Close/Other button (defaults to "Cancel")
        if (options[@"closeButtonLabel"] && ![options[@"closeButtonLabel"] isEqualToString:@""]) {
            [userNotification setValue:@YES forKey:@"_showsButtons"];
            userNotification.otherButtonTitle = options[@"closeButtonLabel"];
        }

        // Reply to the notification with a text field
        if (options[@"response"] && ![options[@"response"] isEqualToString:@""]) {
            userNotification.hasReplyButton = 1;
            userNotification.responsePlaceholder = options[@"mainButtonLabel"];
        }

        // Change the icon of the app in the notification
        if (options[@"appIcon"] && ![options[@"appIcon"] isEqualToString:@""]) {
            NSImage* icon = getImageFromURL(options[@"appIcon"]);
            [userNotification setValue:icon forKey:@"_identityImage"];
            [userNotification setValue:@(false) forKey:@"_identityImageHasBorder"];
        }

        // Change the additional content image
        if (options[@"contentImage"] && ![options[@"contentImage"] isEqualToString:@""]) {
            userNotification.contentImage = getImageFromURL(options[@"contentImage"]);
        }

        // Send or schedule notification
        if (isScheduled) {
            [notificationCenter scheduleNotification:userNotification];
        } else {
            [notificationCenter deliverNotification:userNotification];
        }

        if (!shouldWait) {
            // Block until didDeliverNotification: confirms the async XPC delivery completed,
            // so a fire-and-forget caller can't exit before the notification is shown.
            // Scheduled notifications are excluded — their delivery date is in the future and
            // didDeliverNotification: won't fire until then.  Bounded by a safety timeout so
            // we never hang if the callback is never delivered.
            if (!isScheduled) {
                if ([NSThread isMainThread]) {
                    NSDate* deadline = [NSDate dateWithTimeIntervalSinceNow:kDeliveryTimeoutSecs];
                    while (!rust_notification_is_delivered(notificationId) &&
                           [deadline timeIntervalSinceNow] > 0) {
                        [[NSRunLoop currentRunLoop]
                            runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
                    }
                } else {
                    // Blocks on a timed Condvar; returns once delivered or after 2s.
                    rust_wait_for_delivery(notificationId);
                }
            }
            return;
        }

        // auto-dismiss: notification disappeared from deliveredNotifications without a callback
        BOOL (^wasAutoDismissed)(void) = ^BOOL {
          for (NSUserNotification* n in notificationCenter.deliveredNotifications) {
              if ([n.identifier isEqualToString:identifierString])
                  return NO;
          }
          return YES;
        };

        if ([NSThread isMainThread]) {
            // wait for delivery before checking dismissal. the async delivery window
            // would otherwise look like an immediate dismiss. timeout after 2s so we don't hang
            NSDate* deliveryDeadline = [NSDate dateWithTimeIntervalSinceNow:kDeliveryTimeoutSecs];
            while (!rust_notification_is_delivered(notificationId) && !rust_notification_is_done(notificationId) &&
                   [deliveryDeadline timeIntervalSinceNow] > 0) {
                [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
            }
            // delivery timed out without confirmation -> treat as auto-dismiss
            if (!rust_notification_is_done(notificationId) && !rust_notification_is_delivered(notificationId))
                resolveAutoDismiss(notificationId);

            // keep the run loop spinning; callbacks arrive here and signal when done
            while (!rust_notification_is_done(notificationId)) {
                [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
                if (rust_notification_is_delivered(notificationId) && wasAutoDismissed())
                    resolveAutoDismiss(notificationId);
            }
        } else {
            // callbacks come in on main thread, start poll timer there (#86)
            // copy UUID to NSData so the block owns the lifetime, not the Rust stack
            NSData* notificationIdData = [NSData dataWithBytes:notificationId length:16];
            NSDate* pollStarted = [NSDate date];
            NSTimer* dismissPoll = [NSTimer timerWithTimeInterval:0.5
                                                          repeats:YES
                                                            block:^(NSTimer* t) {
                                                              // wait for delivery before checking dismiss, same logic as main thread
                                                              if (!rust_notification_is_delivered(notificationIdData.bytes)) {
                                                                  if (!rust_notification_is_done(notificationIdData.bytes) && -[pollStarted timeIntervalSinceNow] > kDeliveryTimeoutSecs) {
                                                                      [t invalidate];
                                                                      resolveAutoDismiss(notificationIdData.bytes);
                                                                  }
                                                                  return;
                                                              }
                                                              if (wasAutoDismissed()) {
                                                                  [t invalidate];
                                                                  resolveAutoDismiss(notificationIdData.bytes);
                                                              }
                                                            }];
            [[NSRunLoop mainRunLoop] addTimer:dismissPoll forMode:NSDefaultRunLoopMode];
            // Blocks until rust_notification_auto_dismissed / rust_notification_activated /
            // rust_notification_dismissed signals the Condvar.
            rust_wait_for_notification(notificationId);
            dispatch_async(dispatch_get_main_queue(), ^{
              [dismissPoll invalidate];
            });
        }
    }
}

// Ten actual native owners: eight response listeners plus the existing message
// and incoming-call single-flight submission owners. Unknown records consume
// their original slot forever; no timeout or new UUID permits reuse.
enum { OwnedFree = 0, OwnedBusy = 1, OwnedLive = 2, OwnedUnknown = 3 };
enum { OwnedOk = 0, OwnedCapacity = 1, OwnedInvalid = 2,
       OwnedPreparation = 3, OwnedNativeUncertain = 4 };
enum { StageReserved = 1, StageDelegate = 2, StageCenter = 3,
       StageNotificationAlloc = 4, StageNotificationInit = 5,
       StageStringAlloc = 6, StageStringInit = 7, StageSetIdentifier = 8,
       StageSetTitle = 9, StageSetSubtitle = 10, StageSetBody = 11,
       StageSetSound = 12, StageReleaseString = 13,
       StagePrepared = 14, StageSubmitEntered = 15, StageSubmitted = 16,
       StageWithdrawEntered = 17, StageWithdrawReturned = 18,
       StageReleaseNotification = 19, StageReleaseCenter = 20,
       StageReleased = 21, StagePoolAlloc = 22, StagePoolInit = 23,
       StagePoolDrain = 24, StageThreadCheck = 25 };
#define OWNED_RECORDS 10

typedef struct {
    atomic_uint state;
    uint8_t identity[16];
    uint64_t cookie;
    pthread_t thread;
    uint32_t stage;
    NSUserNotification* notification;
    NSUserNotificationCenter* center;
    NSString* scratch;
    NSAutoreleasePool* pool;
} OwnedRecord;

static OwnedRecord ownedRecords[OWNED_RECORDS];
static atomic_uint_fast64_t ownedNextCookie = 1;
_Static_assert(sizeof(uint_fast64_t) == 8, "owned cookie is exactly 64 bits");

static void ownedResult(MorpheusOwnedResult* out, uint32_t slot,
                        uint32_t status, const OwnedRecord* record) {
    *out = (MorpheusOwnedResult){slot, status, record ? record->cookie : 0,
                                record ? record->stage : 0, 0};
}

// Only the thread which has claimed Busy may read or mutate record metadata.
// Release/Acquire around Free/Live/Unknown publishes all immutable identity and
// original-thread metadata; no scanner reads a record concurrently with reuse.
static OwnedRecord* ownedClaim(uint32_t slot, uint64_t cookie,
                               const uint8_t* identity) {
    if (slot >= OWNED_RECORDS || identity == NULL || cookie == 0) return NULL;
    OwnedRecord* record = &ownedRecords[slot];
    unsigned expected = OwnedLive;
    if (!atomic_compare_exchange_strong_explicit(&record->state, &expected,
            OwnedBusy, memory_order_acq_rel, memory_order_acquire)) return NULL;
    if (record->cookie != cookie || memcmp(record->identity, identity, 16) != 0 ||
        !pthread_equal(record->thread, pthread_self())) {
        atomic_store_explicit(&record->state, OwnedLive, memory_order_release);
        return NULL;
    }
    return record;
}

static void ownedUnknown(OwnedRecord* record, uint32_t slot,
                         MorpheusOwnedResult* out) {
    ownedResult(out, slot, OwnedNativeUncertain, record);
    // Never access native pointers after this store. The SAME record owns the
    // possible references and partial operation; no retry, release or reuse.
    atomic_store_explicit(&record->state, OwnedUnknown, memory_order_release);
}

static BOOL ownedRelease(OwnedRecord* record, uint32_t slot, BOOL withdraw,
                         MorpheusOwnedResult* out) {
    @try {
        if (withdraw && record->notification && record->center) {
            record->stage = StageWithdrawEntered;
            [record->center removeDeliveredNotification:record->notification];
            record->stage = StageWithdrawReturned;
        }
        if (record->scratch) {
            record->stage = StageReleaseString;
            [record->scratch release];
            record->scratch = nil;
        }
        if (record->notification) {
            record->stage = StageReleaseNotification;
            [record->notification release];
            record->notification = nil;
        }
        if (record->center) {
            record->stage = StageReleaseCenter;
            [record->center release];
            record->center = nil;
        }
        if (record->pool) {
            record->stage = StagePoolDrain;
            [record->pool drain];
            record->pool = nil;
        }
        record->stage = StageReleased;
        ownedResult(out, slot, OwnedOk, record);
        atomic_store_explicit(&record->state, OwnedFree, memory_order_release);
        return YES;
    } @catch (id exception) {
        (void)exception; // No exception names, reasons, payloads or text cross ABI.
        ownedUnknown(record, slot, out);
        return NO;
    }
}

static BOOL ownedString(OwnedRecord* record, MorpheusOwnedBytes bytes) {
    record->stage = StageStringAlloc;
    record->scratch = [NSString alloc];
    record->stage = StageStringInit;
    record->scratch = [record->scratch initWithBytes:bytes.bytes
                                            length:(NSUInteger)bytes.length
                                          encoding:NSUTF8StringEncoding];
    return record->scratch != nil;
}

static void ownedStringRelease(OwnedRecord* record) {
    record->stage = StageReleaseString;
    [record->scratch release];
    record->scratch = nil;
}

void morpheus_owned_prepare(const uint8_t* identity, MorpheusOwnedBytes title,
                           MorpheusOwnedBytes subtitle, MorpheusOwnedBytes body,
                           uint32_t sound, MorpheusOwnedResult* out) {
    if (!out) return;
    ownedResult(out, UINT32_MAX, OwnedInvalid, NULL);
    if (!identity || sound > 1 || title.length > 4096 ||
        subtitle.length > 4096 || body.length > 65536 ||
        (!title.bytes && title.length) || (!subtitle.bytes && subtitle.length) ||
        (!body.bytes && body.length)) return;
    uint_fast64_t cookie = atomic_load_explicit(&ownedNextCookie, memory_order_relaxed);
    do {
        if (cookie == UINT64_MAX) { out->status = OwnedCapacity; return; }
    } while (!atomic_compare_exchange_weak_explicit(&ownedNextCookie, &cookie,
                cookie + 1, memory_order_relaxed, memory_order_relaxed));
    uint32_t slot;
    OwnedRecord* record = NULL;
    for (slot = 0; slot < OWNED_RECORDS; ++slot) {
        unsigned expected = OwnedFree;
        if (atomic_compare_exchange_strong_explicit(&ownedRecords[slot].state,
                &expected, OwnedBusy, memory_order_acq_rel, memory_order_acquire)) {
            record = &ownedRecords[slot];
            break;
        }
    }
    if (!record) { out->status = OwnedCapacity; return; }
    memcpy(record->identity, identity, 16);
    record->cookie = cookie;
    record->thread = pthread_self();
    record->stage = StageReserved;
    record->notification = nil;
    record->center = nil;
    record->scratch = nil;
    record->pool = nil;
    @try {
        // Apple requires Cocoa multithreading mode before POSIX workers call
        // Cocoa. No detached initialization thread or silent fallback exists.
        record->stage = StageThreadCheck;
        if (![NSThread isMultiThreaded]) {
            if (ownedRelease(record, slot, NO, out)) out->status = OwnedPreparation;
            return;
        }
        // std worker threads have no ambient Cocoa pool. Its exact allocation
        // and final drain are owned by this same record/thread as well.
        record->stage = StagePoolAlloc;
        record->pool = [NSAutoreleasePool alloc];
        record->stage = StagePoolInit;
        record->pool = [record->pool init];
        if (!record->pool) {
            if (ownedRelease(record, slot, NO, out)) out->status = OwnedPreparation;
            return;
        }
        record->stage = StageDelegate;
        setupDelegate();
        record->stage = StageCenter;
        record->center = [NSUserNotificationCenter defaultUserNotificationCenter];
        record->center = [record->center retain];
        record->stage = StageNotificationAlloc;
        record->notification = [NSUserNotification alloc];
        record->stage = StageNotificationInit;
        record->notification = [record->notification init];
        if (!record->center || !record->notification) {
            if (ownedRelease(record, slot, NO, out)) out->status = OwnedPreparation;
            return;
        }
        static const char prefix[] = "morpheus-owned:";
        static const char hex[] = "0123456789abcdef";
        uint8_t identifier[sizeof(prefix) - 1 + 32];
        memcpy(identifier, prefix, sizeof(prefix) - 1);
        for (size_t i = 0; i < 16; ++i) {
            identifier[sizeof(prefix) - 1 + 2*i] = hex[identity[i] >> 4];
            identifier[sizeof(prefix) + 2*i] = hex[identity[i] & 15];
        }
        if (!ownedString(record, (MorpheusOwnedBytes){identifier, sizeof(identifier)})) {
            if (ownedRelease(record, slot, NO, out)) out->status = OwnedPreparation;
            return;
        }
        record->stage = StageSetIdentifier;
        record->notification.identifier = record->scratch;
        ownedStringRelease(record);
        MorpheusOwnedBytes texts[] = {title, subtitle, body};
        for (unsigned i = 0; i < 3; ++i) {
            if (!ownedString(record, texts[i])) {
                if (ownedRelease(record, slot, NO, out)) out->status = OwnedPreparation;
                return;
            }
            record->stage = i == 0 ? StageSetTitle : i == 1 ? StageSetSubtitle : StageSetBody;
            if (i == 0) record->notification.title = record->scratch;
            else if (i == 1) record->notification.subtitle = record->scratch;
            else record->notification.informativeText = record->scratch;
            ownedStringRelease(record);
        }
        record->stage = StageSetSound;
        record->notification.soundName = sound ? NSUserNotificationDefaultSoundName : nil;
        record->stage = StagePrepared;
        ownedResult(out, slot, OwnedOk, record);
        atomic_store_explicit(&record->state, OwnedLive, memory_order_release);
    } @catch (id exception) {
        (void)exception;
        ownedUnknown(record, slot, out);
    }
}

void morpheus_owned_submit(uint32_t slot, uint64_t cookie, const uint8_t* identity,
                          MorpheusOwnedResult* out) {
    if (!out) return;
    ownedResult(out, slot, OwnedNativeUncertain, NULL);
    OwnedRecord* record = ownedClaim(slot, cookie, identity);
    if (!record) return;
    @try {
        record->stage = StageSubmitEntered;
        [record->center deliverNotification:record->notification];
        record->stage = StageSubmitted;
        ownedResult(out, slot, OwnedOk, record);
        atomic_store_explicit(&record->state, OwnedLive, memory_order_release);
    } @catch (id exception) {
        (void)exception;
        ownedUnknown(record, slot, out);
    }
}

void morpheus_owned_retire(uint32_t slot, uint64_t cookie, const uint8_t* identity,
                          uint32_t withdraw, MorpheusOwnedResult* out) {
    if (!out) return;
    ownedResult(out, slot, OwnedNativeUncertain, NULL);
    if (withdraw > 1) return;
    OwnedRecord* record = ownedClaim(slot, cookie, identity);
    if (record) ownedRelease(record, slot, withdraw != 0, out);
}

void morpheus_owned_abandon(uint32_t slot, uint64_t cookie, const uint8_t* identity) {
    OwnedRecord* record = ownedClaim(slot, cookie, identity);
    if (record) {
        atomic_store_explicit(&record->state, OwnedUnknown, memory_order_release);
    }
}

// No owned callback passes native strings, KVC values, object pointers or a Rust
// context. A malformed owned prefix is consumed, never routed to the old bridge.
static BOOL ownedRoute(NSUserNotification* notification, uint32_t event) {
    uint8_t identity[16];
    @try {
        NSString* identifier = notification.identifier;
        static const char prefix[] = "morpheus-owned:";
        if (!identifier || ![identifier hasPrefix:@"morpheus-owned:"]) return NO;
        if (identifier.length != sizeof(prefix) - 1 + 32) return YES;
        for (NSUInteger i = 0; i < 16; ++i) {
            unsigned value = 0;
            for (NSUInteger j = 0; j < 2; ++j) {
                unichar c = [identifier characterAtIndex:sizeof(prefix) - 1 + 2*i + j];
                if (c < '0' || (c > '9' && c < 'a') || c > 'f') return YES;
                value = value * 16 + (c <= '9' ? c - '0' : c - 'a' + 10);
            }
            identity[i] = (uint8_t)value;
        }
        if (event == 0) {
            @try {
                event = notification.activationType == NSUserNotificationActivationTypeContentsClicked ? 0 : 1;
            } @catch (id exception) {
                (void)exception;
                event = 2;
            }
        }
        morpheus_owned_response(identity, event);
        return YES;
    } @catch (id exception) {
        (void)exception;
        // Failure to obtain a correlation identity cannot authorize a click.
        // The actual owned listener remains cancellable by its original owner.
        return YES;
    }
}

@implementation NotificationCenterDelegate

- (void)userNotificationCenter:(NSUserNotificationCenter*)center
        didDeliverNotification:(NSUserNotification*)notification {
    // Delivery is not a worker/callback join or an owned completion receipt.
    // Consume owned notifications without consulting the legacy UUID map.
    @try { if ([notification.identifier hasPrefix:@"morpheus-owned:"]) return; }
    @catch (id exception) { (void)exception; return; }
    unsigned char bytes[16];
    uuidBytesFromNotification(notification, bytes);
    rust_notification_delivered(bytes);
}

- (void)userNotificationCenter:(NSUserNotificationCenter*)center
       didActivateNotification:(NSUserNotification*)notification {
    if (ownedRoute(notification, 0)) return;
    unsigned char bytes[16];
    uuidBytesFromNotification(notification, bytes);

    uint8_t activationType = 0;
    const char* actionValue = NULL;
    int64_t actionValueIndex = -1;

    switch (notification.activationType) {
        case NSUserNotificationActivationTypeActionButtonClicked:
        case NSUserNotificationActivationTypeAdditionalActionClicked: {
            NSArray* altTitles = [(NSObject*)notification valueForKey:@"_alternateActionButtonTitles"];
            if ([altTitles count] > 1) {
                NSNumber* altIdx = [(NSObject*)notification valueForKey:@"_alternateActionIndex"];
                unsigned long long idx = [altIdx unsignedLongLongValue];
                if (idx == (unsigned long long)LONG_MAX) {
                    actionValue = [notification.actionButtonTitle UTF8String];
                } else {
                    actionValue = [altTitles[idx] UTF8String];
                    actionValueIndex = (int64_t)idx;
                }
            } else {
                actionValue = [notification.actionButtonTitle UTF8String];
            }
            activationType = 1;
            break;
        }
        case NSUserNotificationActivationTypeContentsClicked:
            activationType = 2;
            break;
        case NSUserNotificationActivationTypeReplied:
            activationType = 3;
            actionValue = [notification.response.string UTF8String];
            break;
        case NSUserNotificationActivationTypeNone:
        default:
            break;
    }

    rust_notification_activated(bytes, activationType, actionValue, actionValueIndex);
    [center removeDeliveredNotification:notification];
}

- (void)userNotificationCenter:(NSUserNotificationCenter*)center
               didDismissAlert:(NSUserNotification*)notification {
    if (ownedRoute(notification, 1)) return;
    unsigned char bytes[16];
    uuidBytesFromNotification(notification, bytes);
    rust_notification_dismissed(bytes, [notification.otherButtonTitle UTF8String]);
    [center removeDeliveredNotification:notification];
}

+ (instancetype)sharedDelegate {
    static NotificationCenterDelegate* instance = nil;
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
      instance = [[NotificationCenterDelegate alloc] init];
      [NSUserNotificationCenter defaultUserNotificationCenter].delegate = instance;
    });
    return instance;
}
@end

void setupDelegate(void) {
    [NotificationCenterDelegate sharedDelegate];
}
