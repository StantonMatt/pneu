// Private UI-child bridge. All session mutation/start/stop runs on one serial
// queue; Vision and frame copies run on a separate, discard-late-frames queue.
#import <AVFoundation/AVFoundation.h>
#import <Vision/Vision.h>
#import <stdatomic.h>

typedef void (*PneuCameraEvent)(uint64_t, int, const uint8_t *, size_t, uint32_t, uint32_t);
typedef bool (*PneuCameraFilter)(const uint8_t *, size_t);
static atomic_uint sessionsRunning = 0;

static dispatch_group_t pendingStops(void) {
    static dispatch_group_t group;
    static dispatch_once_t once;
    dispatch_once(&once, ^{ group = dispatch_group_create(); });
    return group;
}
static dispatch_queue_t sessionQueue(void) {
    static dispatch_queue_t queue;
    static dispatch_once_t once;
    dispatch_once(&once, ^{ queue = dispatch_queue_create("pneu.camera.session", DISPATCH_QUEUE_SERIAL); });
    return queue;
}

// This is also the native still-image test's decoder. No camera or permission
// access is needed to exercise Vision's actual QR request.
static NSArray<NSString *> *decodeQR(CVPixelBufferRef buffer, NSError **error) {
    VNDetectBarcodesRequest *request = [VNDetectBarcodesRequest new];
    request.symbologies = @[VNBarcodeSymbologyQR];
    VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCVPixelBuffer:buffer options:@{}];
    if (![handler performRequests:@[request] error:error]) return nil;
    NSMutableArray<NSString *> *payloads = [NSMutableArray new];
    for (VNBarcodeObservation *observation in request.results) {
        NSString *payload = observation.payloadStringValue;
        if (payload && [payload lengthOfBytesUsingEncoding:NSUTF8StringEncoding] <= 8193) {
            [payloads addObject:payload];
        }
    }
    return payloads;
}

static NSArray<AVCaptureDevice *> *cameras(void) {
    return [AVCaptureDeviceDiscoverySession
        discoverySessionWithDeviceTypes:@[AVCaptureDeviceTypeBuiltInWideAngleCamera,
                                          AVCaptureDeviceTypeExternal]
        mediaType:AVMediaTypeVideo position:AVCaptureDevicePositionUnspecified].devices;
}

bool pneu_camera_available(void) {
    @autoreleasepool { return cameras().count > 0; }
}
int pneu_camera_authorization(void) {
    @autoreleasepool { return (int)[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo]; }
}
void pneu_camera_request_authorization(uint64_t generation, PneuCameraEvent callback) {
    @autoreleasepool {
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
            (void)granted;
            @autoreleasepool {
                callback(generation, 4, NULL, 0, (uint32_t)pneu_camera_authorization(), 0);
            }
        }];
    }
}

@interface PneuCamera : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate> {
    atomic_bool _active;
}
@property(nonatomic) uint64_t generation;
@property(nonatomic) PneuCameraEvent callback;
@property(nonatomic) PneuCameraFilter filter;
@property(nonatomic, strong) dispatch_queue_t control;
@property(nonatomic, strong) dispatch_queue_t frames;
@property(nonatomic, strong) AVCaptureSession *session;
@property(nonatomic, strong) AVCaptureVideoDataOutput *output;
@property(nonatomic, strong) NSMutableArray *observers;
@property(nonatomic) CFTimeInterval lastPreview;
@property(nonatomic) CFTimeInterval lastDecode;
@property(nonatomic) BOOL countedRunning;
- (void)start;
- (void)stop;
- (void)invalidate;
@end

@implementation PneuCamera
- (instancetype)init {
    if ((self = [super init])) {
        atomic_init(&_active, true);
        // Reopening waits behind the previous session's queued stop.
        _control = sessionQueue();
        _frames = dispatch_queue_create("pneu.camera.frames", DISPATCH_QUEUE_SERIAL);
        _observers = [NSMutableArray new];
    }
    return self;
}
- (void)invalidate { atomic_store(&_active, false); }
- (void)stop {
    // Only on control. Drain callbacks before dropping the delegate/session.
    [self invalidate];
    [self.output setSampleBufferDelegate:nil queue:NULL];
    [self.session stopRunning];
    if (self.countedRunning) {
        atomic_fetch_sub(&sessionsRunning, 1);
        self.countedRunning = NO;
    }
    for (id observer in self.observers) [[NSNotificationCenter defaultCenter] removeObserver:observer];
    [self.observers removeAllObjects];
    dispatch_sync(self.frames, ^{});
    self.output = nil;
    self.session = nil;
}
- (void)fail {
    [self stop];
    self.callback(self.generation, 3, NULL, 0, 0, 0);
}
- (void)start {
    @autoreleasepool {
        if (!atomic_load(&_active)) return;
        AVCaptureDevice *device = cameras().firstObject;
        NSError *error = nil;
        AVCaptureDeviceInput *input = device ? [AVCaptureDeviceInput deviceInputWithDevice:device error:&error] : nil;
        if (!input || error) { [self fail]; return; }
        self.session = [AVCaptureSession new];
        [self.session beginConfiguration];
        if ([self.session canSetSessionPreset:AVCaptureSessionPreset640x480]) {
            self.session.sessionPreset = AVCaptureSessionPreset640x480;
        }
        self.output = [AVCaptureVideoDataOutput new];
        self.output.videoSettings = @{(id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA)};
        self.output.alwaysDiscardsLateVideoFrames = YES;
        [self.output setSampleBufferDelegate:self queue:self.frames];
        if (![self.session canAddInput:input] || ![self.session canAddOutput:self.output]) {
            [self.session commitConfiguration];
            [self fail]; return;
        }
        [self.session addInput:input];
        [self.session addOutput:self.output];
        [self.session commitConfiguration];
        __weak PneuCamera *weakSelf = self;
        for (NSNotificationName name in @[AVCaptureSessionRuntimeErrorNotification,
                                           AVCaptureSessionWasInterruptedNotification,
                                           AVCaptureDeviceWasDisconnectedNotification]) {
            id observer = [[NSNotificationCenter defaultCenter] addObserverForName:name
                object:([name isEqual:AVCaptureDeviceWasDisconnectedNotification] ? device : self.session)
                queue:nil usingBlock:^(NSNotification *notification) {
                    (void)notification;
                    PneuCamera *camera = weakSelf;
                    if (!camera) return;
                    [camera invalidate];
                    dispatch_async(camera.control, ^{ @autoreleasepool { [camera fail]; } });
                }];
            [self.observers addObject:observer];
        }
        if (atomic_load(&_active)) [self.session startRunning];
        if (!atomic_load(&_active)) { [self stop]; return; }
        if (!self.session.isRunning) [self fail];
        else {
            self.countedRunning = YES;
            atomic_fetch_add(&sessionsRunning, 1);
        }
    }
}
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample
        fromConnection:(AVCaptureConnection *)connection {
    @autoreleasepool {
        if (!atomic_load(&_active)) return;
        CVPixelBufferRef buffer = CMSampleBufferGetImageBuffer(sample);
        size_t width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer);
        // Bound preview copies and Vision's work even for an unusual camera.
        if (!width || !height || width > 1280 || height > 720) {
            [self invalidate];
            dispatch_async(self.control, ^{ @autoreleasepool { [self fail]; } });
            return;
        }
        CFTimeInterval now = CFAbsoluteTimeGetCurrent();
        if (now - self.lastPreview >= 0.1) {
            self.lastPreview = now;
            if (CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
                [self invalidate];
                dispatch_async(self.control, ^{ @autoreleasepool { [self fail]; } });
                return;
            }
            size_t stride = CVPixelBufferGetBytesPerRow(buffer);
            const uint8_t *base = CVPixelBufferGetBaseAddress(buffer);
            if (base && stride >= width * 4) {
                NSMutableData *pixels = [NSMutableData dataWithLength:width * height * 4];
                for (size_t y = 0; y < height; ++y) {
                    memcpy((uint8_t *)pixels.mutableBytes + y * width * 4, base + y * stride, width * 4);
                }
                self.callback(self.generation, 1, pixels.bytes, pixels.length, (uint32_t)width, (uint32_t)height);
            }
            CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
        }
        if (now - self.lastDecode < 0.3 || !atomic_load(&_active)) return;
        self.lastDecode = now;
        NSError *error = nil;
        NSArray<NSString *> *payloads = decodeQR(buffer, &error);
        if (error) {
            [self invalidate];
            dispatch_async(self.control, ^{ @autoreleasepool { [self fail]; } });
            return;
        }
        for (NSString *payload in payloads) {
            NSData *utf8 = [payload dataUsingEncoding:NSUTF8StringEncoding];
            if (!self.filter(utf8.bytes, utf8.length)) continue;
            if (!atomic_exchange(&_active, false)) return;
            // Stop the device before publishing the link to the join flow.
            dispatch_async(self.control, ^{
                @autoreleasepool {
                    [self stop];
                    self.callback(self.generation, 2, utf8.bytes, utf8.length, 0, 0);
                }
            });
            return;
        }
    }
}
@end

void *pneu_camera_start(uint64_t generation, PneuCameraEvent callback, PneuCameraFilter filter) {
    @autoreleasepool {
        PneuCamera *camera = [PneuCamera new];
        camera.generation = generation;
        camera.callback = callback;
        camera.filter = filter;
        dispatch_async(camera.control, ^{ [camera start]; });
        return (__bridge_retained void *)camera;
    }
}
void pneu_camera_stop(void *handle, bool wait) {
    @autoreleasepool {
        PneuCamera *camera = (__bridge_transfer PneuCamera *)handle;
        [camera invalidate];
        if (wait) dispatch_sync(camera.control, ^{ @autoreleasepool { [camera stop]; } });
        else {
            dispatch_group_enter(pendingStops());
            dispatch_async(camera.control, ^{
                @autoreleasepool { [camera stop]; }
                dispatch_group_leave(pendingStops());
            });
        }
    }
}
void pneu_camera_wait_for_stops(void) { dispatch_group_wait(pendingStops(), DISPATCH_TIME_FOREVER); }
bool pneu_camera_is_running(void) { return atomic_load(&sessionsRunning) != 0; }

bool pneu_camera_decode_bgra(const uint8_t *bytes, size_t length, uint32_t width, uint32_t height,
        PneuCameraEvent callback) {
    @autoreleasepool {
        if (!width || !height || width > 1280 || height > 720 || length != (size_t)width * height * 4) return false;
        CVPixelBufferRef buffer = NULL;
        if (CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA,
                NULL, &buffer) != kCVReturnSuccess) return false;
        if (CVPixelBufferLockBaseAddress(buffer, 0) != kCVReturnSuccess) { CVPixelBufferRelease(buffer); return false; }
        uint8_t *base = CVPixelBufferGetBaseAddress(buffer);
        size_t stride = CVPixelBufferGetBytesPerRow(buffer);
        for (size_t y = 0; y < height; ++y) memcpy(base + y * stride, bytes + y * width * 4, width * 4);
        CVPixelBufferUnlockBaseAddress(buffer, 0);
        NSError *error = nil;
        NSArray<NSString *> *payloads = decodeQR(buffer, &error);
        for (NSString *payload in payloads) {
            NSData *utf8 = [payload dataUsingEncoding:NSUTF8StringEncoding];
            callback(0, 2, utf8.bytes, utf8.length, 0, 0);
        }
        CVPixelBufferRelease(buffer);
        return error == nil;
    }
}
